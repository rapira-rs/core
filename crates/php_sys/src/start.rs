use log::{error, info, trace};
use std::cell::RefCell;
use std::marker::PhantomData;
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;
use tokio::sync::mpsc::{self, Receiver, Sender};
use types::Job;

use crate::rapira_worker::{WorkerExit, rapira_worker};
use crate::scoreboard::{Scoreboard, ScoreboardSnapshot, sb_set};
use crate::{classic_worker::classic_worker, types::Mode, *};

thread_local! {
    // Owned by the single PHP worker thread; installed once by worker_main and
    // reused across worker restarts on the same OS thread. The fork-based pool
    // keeps this shape: one receiver per worker process.
    static JOB_RX: RefCell<Option<Receiver<Job>>> = const { RefCell::new(None) };
}

pub struct Rapira {
    pub(crate) intake: Option<Sender<Job>>,
    worker: Option<JoinHandle<()>>,
    pub scoreboard: Arc<Scoreboard>,
    _not_send: PhantomData<*const ()>, // !Send + !Sync, to prevent dropping from a foreign thread (which would be UB)
}

impl Rapira {
    pub fn start(mode: Mode) -> anyhow::Result<Self> {
        info!(target: "rapira", "booting with mode: {mode:?}");
        let scoreboard: Arc<Scoreboard> = Arc::new(Scoreboard::default());

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

        let (intake, intake_rx) = mpsc::channel::<Job>(1024);

        trace!(target: "rapira", "spawning worker thread");
        let worker: JoinHandle<()> = {
            let scoreboard = scoreboard.clone();
            thread::spawn(move || worker_main(scoreboard, mode, intake_rx))
        };

        Ok(Self {
            intake: Some(intake),
            worker: Some(worker),
            scoreboard,
            _not_send: PhantomData,
        })
    }

    pub fn shutdown(self) {
        info!(target: "rapira", "shutdown is noop, deinitialize in Drop");
    }

    pub fn scoreboard(&self) -> ScoreboardSnapshot {
        self.scoreboard.snapshot()
    }
}

impl Drop for Rapira {
    fn drop(&mut self) {
        info!(target: "rapira", "shutting down, dropping");
        self.intake = None;
        let Some(worker) = self.worker.take() else {
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
            return;
        }

        let _ = worker.join();
        unsafe {
            php_module_shutdown();
            sapi_shutdown();
        }
    }
}

fn worker_main(board: Arc<Scoreboard>, mode: Mode, rx: Receiver<Job>) {
    sb_set(board);
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

/// Block for the next job (shutdown-aware): `None` means the intake channel
/// closed — every `Sender`/`RapiraHandle` was dropped, i.e. Rapira is shutting
/// down. The single place the receiver is consumed; the classic loop,
/// worker-mode `next_job`, and the boot-failure drain all go through here.
pub(crate) fn pull_job() -> Option<Job> {
    // Holding the RefCell borrow across the blocking recv is safe: the thread is
    // parked inside it and pull_job is never re-entered on this thread.
    JOB_RX.with_borrow_mut(|rx| rx.as_mut()?.blocking_recv())
}
