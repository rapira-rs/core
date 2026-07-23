use log::{error, info, trace};
use std::cell::RefCell;
use std::marker::PhantomData;
use std::thread;
use std::thread::JoinHandle;
use tokio::sync::mpsc::{self, Receiver, Sender};
use types::Job;

use crate::quota::{self, WorkerHooks};
use crate::rapira_worker::{WorkerExit, rapira_worker};
use crate::scoreboard::{Event, ScoreboardSnapshot, sb_set, sb_update};
use crate::{classic_worker::classic_worker, types::Mode, *};

thread_local! {
    // Owned by the single PHP worker thread; installed once by worker_main and
    // reused across worker restarts on the same OS thread. One receiver per
    // worker process in the fork-based pool.
    static JOB_RX: RefCell<Option<Receiver<Job>>> = const { RefCell::new(None) };
}

/// Proof that sapi_startup + php_module_startup (MINIT) succeeded in THIS
/// process. `!Send`: created and dropped on the module-startup thread. Drop
/// runs the module teardown, so exactly one place holds it:
/// - fused path: inside `Rapira` (single-process semantics);
/// - fork mode: the master, for its whole life (opcache SHM mmap'd at MINIT is
///   shared by every fork). Forked workers NEVER drop it — they leave via
///   process exit, which skips Drop; the master owns the single engine teardown.
pub struct PhpModule {
    _not_send: PhantomData<*const ()>,
}

impl Drop for PhpModule {
    fn drop(&mut self) {
        unsafe {
            php_module_shutdown();
            sapi_shutdown();
        }
    }
}

pub struct Rapira {
    pub(crate) intake: Option<Sender<Job>>,
    worker: Option<JoinHandle<()>>,
    /// Some = fused/private board (tests, single-process); None = external slot.
    board: Option<rapira_scoreboard::Scoreboard>,
    /// Some = this value owns module teardown (fused path); None = worker flavor.
    module: Option<PhpModule>,
    _not_send: PhantomData<*const ()>, // !Send + !Sync, to prevent dropping from a foreign thread (which would be UB)
}

impl Rapira {
    /// Master-side boot: MINIT only, on the calling (still single-threaded)
    /// thread. No worker thread, no channels. Once per process, pre-fork.
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
        Ok(PhpModule {
            _not_send: PhantomData,
        })
    }

    /// Worker-side (post-fork) start: job channel + the single PHP worker
    /// thread, against the engine inherited from `boot_master` in the parent.
    /// No module startup here; the returned value never tears the module down.
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

        let (intake, intake_rx) = mpsc::channel::<Job>(1024);

        trace!(target: "rapira", "spawning worker thread");
        let worker: JoinHandle<()> = thread::spawn(move || {
            sb_set(slot);
            quota::install(max_requests, on_quota, on_unhealthy);
            worker_main(mode, intake_rx)
        });

        Ok(Self {
            intake: Some(intake),
            worker: Some(worker),
            board,
            module: None,
            _not_send: PhantomData,
        })
    }

    /// Fused single-process boot — module + worker in one. The in-process
    /// integration tests run through here.
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
            // No thread to wait for; preserve "no teardown" exactly.
            std::mem::forget(self.module.take());
            return;
        };

        // The worker may never come back: the Zend timer only fires when max_execution_time > 0
        // (and only exists on Linux/FreeBSD), and a leaked RapiraHandle keeps the intake open,
        // parking the worker in pull_job. Bound the wait and, if it is still running, skip
        // the C teardown - php_module_shutdown on a live PHP thread is UB - and let process
        // exit reclaim it.
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

fn worker_main(mode: Mode, rx: Receiver<Job>) {
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
            Mode::Worker(script) => rapira_worker(script.clone()),
        };
        if matches!(exit, WorkerExit::Closed) {
            break;
        }
        // Restart: loop back on this same OS thread — JOB_RX stays installed and
        // the worker re-bootstraps with a fresh request cycle.
    }
}

/// Jobs waiting in the intake channel. 0 when the receiver is not installed or
/// is momentarily borrowed by `pull_job` — a best-effort gauge, never a lock.
pub(crate) fn intake_depth() -> u64 {
    JOB_RX
        .try_with(|cell| {
            cell.try_borrow()
                .ok()
                .and_then(|rx| rx.as_ref().map(|r| r.len() as u64))
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// Block for the next job (shutdown-aware): `None` means the intake channel
/// closed — every `Sender`/`RapiraHandle` was dropped, i.e. Rapira is shutting
/// down. The single place the receiver is consumed; the classic loop,
/// worker-mode `next_job`, and the boot-failure drain all go through here.
/// Also the scoreboard idle/active hinge: parked here = spare capacity.
pub(crate) fn pull_job() -> Option<Job> {
    // Holding the RefCell borrow across the blocking recv is safe: the thread is
    // parked inside it and pull_job is never re-entered on this thread.
    JOB_RX.with_borrow_mut(|rx| {
        let rx = rx.as_mut()?;
        sb_update(Event::Idle);
        let job = rx.blocking_recv();
        if job.is_some() {
            sb_update(Event::Active);
        }
        job
    })
}
