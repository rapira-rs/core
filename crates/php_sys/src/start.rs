use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{error, info, trace};
use types::Job;

use crate::quota::{self, WorkerHooks};
use crate::rapira_worker::{WorkerExit, rapira_worker};
use crate::scoreboard::{Event, ScoreboardSnapshot, sb_set, sb_update};
use crate::{classic_worker::classic_worker, types::Mode, *};

thread_local! {
    static JOB_RX: RefCell<Option<JobRx>> = const { RefCell::new(None) };
}

pub(crate) struct Intake {
    pub(crate) tx: SyncSender<Job>,
    pub(crate) pending: Arc<AtomicUsize>,
}

struct JobRx {
    rx: Receiver<Job>,
    pending: Arc<AtomicUsize>,
}

pub struct PhpModule {}

impl Drop for PhpModule {
    fn drop(&mut self) {
        unsafe {
            php_module_shutdown();
            sapi_shutdown();
        }
    }
}

pub struct Rapira {
    pub(crate) intake: Option<Intake>,
    worker: Option<JoinHandle<()>>,
    board: Option<rapira_scoreboard::Scoreboard>,
    module: Option<PhpModule>,
}

impl Rapira {
    pub fn boot_master() -> anyhow::Result<PhpModule> {
        let mut module: _sapi_module_struct = module::build_sapi_module();
        let started: bool = unsafe {
            rapira_process_init();
            sapi_startup(&mut module);
            module
                .startup
                .is_some_and(|start| start(&mut module) == SUCCESS)
        };

        if !started {
            error!(target: "rapira", "php_module_startup failed, shutting down");
            unsafe {
                php_module_shutdown();
                sapi_shutdown();
            }
            return Err(anyhow::anyhow!("php_module_startup failed"));
        }
        Ok(PhpModule {})
    }

    // worker side part
    pub fn start_worker(mode: Mode, hooks: WorkerHooks) -> anyhow::Result<Self> {
        let WorkerHooks {
            max_requests,
            on_quota,
            on_unhealthy,
            slot,
        } = hooks;
        let (board, slot) = match slot {
            Some(s) => (None, s),
            None => {
                let board = rapira_scoreboard::Scoreboard::create(1)?;
                (Some(board), board.slot(0).expect("slot 0 exists"))
            }
        };
        slot.bind(std::process::id());
        let pending = Arc::new(AtomicUsize::new(0));
        let (intake_tx, intake_rx) = sync_channel::<Job>(1024);
        let intake = Intake {
            tx: intake_tx,
            pending: pending.clone(),
        };

        // SAFETY: safe, trust me, I'm a developer
        unsafe {
            crate::rapira_mode = match &mode {
                Mode::Classic => RAPIRA_MODE_CLASSIC,
                Mode::Dispatcher(_) => RAPIRA_MODE_DISPATCHER,
            } as c_int;
        };

        trace!(target: "rapira", "spawning worker thread");
        let worker: JoinHandle<()> = thread::spawn(move || {
            sb_set(slot);
            quota::install(max_requests, on_quota, on_unhealthy);
            worker_main(
                mode,
                JobRx {
                    rx: intake_rx,
                    pending,
                },
            )
        });

        Ok(Self {
            intake: Some(intake),
            worker: Some(worker),
            board,
            module: None,
        })
    }

    pub fn start(mode: Mode) -> anyhow::Result<Self> {
        info!(target: "rapira", "booting with mode: {mode:?}");
        let module = Self::boot_master()?;
        let mut rapira = Self::start_worker(mode, WorkerHooks::default())?;
        rapira.module = Some(module);
        Ok(rapira)
    }

    pub fn shutdown(self) {
        info!(target: "rapira", "shutdown is noop, deinitialize in Drop");
    }

    pub fn scoreboard(&self) -> ScoreboardSnapshot {
        match &self.board {
            Some(board) => crate::scoreboard::snapshot(board),
            None => ScoreboardSnapshot::default(),
        }
    }
}

impl Drop for Rapira {
    fn drop(&mut self) {
        info!(target: "rapira", "shutting down, dropping");
        self.intake = None;
        let Some(worker) = self.worker.take() else {
            std::mem::forget(self.module.take());
            return;
        };

        // https://www.php.net/manual/en/info.configuration.php#ini.max-execution-time
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !worker.is_finished() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !worker.is_finished() {
            error!(
                target: "rapira",
                "worker still running after grace; skipping PHP module shutdown to avoid UB on a live thread"
            );
            // Leak the module teardown; process exit reclaims it.
            std::mem::forget(self.module.take());
            return;
        }

        let _ = worker.join();
        // Fused: module teardown, same order as before the split; worker flavor: no-op.
        drop(self.module.take());
    }
}

fn worker_main(mode: Mode, rx: JobRx) {
    JOB_RX.with_borrow_mut(|slot| *slot = Some(rx));
    loop {
        unsafe {
            // https://github.com/php/php-src/pull/9104
            // NTS inits module and request on different threads, so the call stack must be
            // re-initialized here.
            rapira_init_call_stack();
        };
        let exit: WorkerExit = match &mode {
            Mode::Classic => {
                classic_worker();
                WorkerExit::Closed
            }
            Mode::Dispatcher(script) => rapira_worker(script.clone()),
        };
        if matches!(exit, WorkerExit::Closed) {
            break;
        }
    }
}

pub(crate) fn pull_job() -> Option<Job> {
    JOB_RX.with_borrow_mut(|slot| {
        let job_r = slot.as_mut()?;
        sb_update(Event::Idle);
        let job = job_r.rx.recv().ok();
        if job.is_some() {
            job_r.pending.fetch_sub(1, Ordering::Relaxed);
            sb_update(Event::Active);
        }
        job
    })
}

pub(crate) enum Pulled {
    // Boxed: a Job is ~600 bytes and the other variants are empty
    Job(Box<Job>),
    Timeout,
    Empty,
    Closed,
}

/// receive(-1) / receive(n): block up to `timeout` (None = forever).
pub(crate) fn pull_job_wait(timeout: Option<Duration>) -> Pulled {
    JOB_RX.with_borrow_mut(|slot| {
        let Some(job_r) = slot.as_mut() else {
            return Pulled::Closed;
        };
        sb_update(Event::Idle);
        let got = match timeout {
            None => job_r.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(t) => job_r.rx.recv_timeout(t),
        };
        match got {
            Ok(job) => {
                job_r.pending.fetch_sub(1, Ordering::Relaxed);
                sb_update(Event::Active);
                Pulled::Job(Box::new(job))
            }
            Err(RecvTimeoutError::Timeout) => Pulled::Timeout,
            Err(RecvTimeoutError::Disconnected) => Pulled::Closed,
        }
    })
}

/// tryReceive() / receive(0): never blocks. Idle is reported here too — a
/// polling worker must refresh last_activity_ms or the master watchdog
/// TERMs it as a stuck request (master/src/events.rs:509-517).
pub(crate) fn pull_job_try() -> Pulled {
    JOB_RX.with_borrow_mut(|slot| {
        let Some(job_r) = slot.as_mut() else {
            return Pulled::Closed;
        };
        sb_update(Event::Idle);
        match job_r.rx.try_recv() {
            Ok(job) => {
                job_r.pending.fetch_sub(1, Ordering::Relaxed);
                sb_update(Event::Active);
                Pulled::Job(Box::new(job))
            }
            Err(TryRecvError::Empty) => Pulled::Empty,
            Err(TryRecvError::Disconnected) => Pulled::Closed,
        }
    })
}

pub(crate) fn pending_depth() -> usize {
    JOB_RX.with_borrow(|slot| {
        slot.as_ref()
            .map_or(0, |job_r| job_r.pending.load(Ordering::Relaxed))
    })
}
