use log::{error, info, trace, warn};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use tokio::sync::mpsc::{self, Receiver, Sender};
use types::Job;

use crate::rapira_worker::{WorkerExit, rapira_worker};
use crate::scoreboard::{Scoreboard, ScoreboardSnapshot, sb_set};
use crate::{classic_worker::classic_worker, types::Mode, *};

pub(crate) type JobRx = Arc<Mutex<Receiver<Job>>>;

pub struct Rapira {
    pub(crate) intake: Option<Sender<Job>>,
    workers: Vec<JoinHandle<()>>,
    pub scoreboard: Arc<Scoreboard>,
    _not_send: PhantomData<*const ()>, // !Send + !Sync, to prevent dropping from a foreign thread (which would be UB)
}

impl Rapira {
    pub fn start(mode: Mode, req_threads: usize) -> anyhow::Result<Self> {
        info!(target: "rapira", "booting with mode: {mode:?}, threads: {req_threads}");
        // NTS runs a single PHP interpreter; the fork-based pool scales with processes.
        let num_threads: usize = {
            if req_threads > 1 {
                warn!(target: "rapira", "rapira runs a single PHP worker thread; threads={req_threads} ignored");
            }
            1
        };
        let scoreboard: Arc<Scoreboard> = Scoreboard::new(num_threads);

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
        let rx: JobRx = Arc::new(Mutex::new(intake_rx));

        let workers: Vec<JoinHandle<()>> = (0..num_threads)
            .map(|id| {
                let (rx, mode, scoreboard) = (rx.clone(), mode.clone(), scoreboard.clone());
                trace!(target: "rapira", "spawning worker thread");
                thread::spawn(move || worker_main(id, scoreboard, mode, rx))
            })
            .collect();

        Ok(Self {
            intake: Some(intake),
            workers,
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
        let workers: Vec<JoinHandle<()>> = std::mem::take(&mut self.workers);

        // A worker may never come back: the Zend timer only fires when max_execution_time > 0
        // (and only exists on Linux/FreeBSD), and a leaked RapiraHandle keeps the intake open,
        // parking workers in pull_job. Bound the wait and, if a worker is still running, skip
        // the C teardown - php_module_shutdown on a live PHP thread is UB - and let process
        // exit reclaim it.
        // https://www.php.net/manual/en/info.configuration.php#ini.max-execution-time
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && workers.iter().any(|w| !w.is_finished()) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if workers.iter().any(|w| !w.is_finished()) {
            error!(
                target: "rapira",
                "worker still running after grace; skipping PHP module shutdown to avoid UB on a live thread"
            );
            return;
        }

        for w in workers {
            let _ = w.join();
        }
        unsafe {
            php_module_shutdown();
            sapi_shutdown();
        }
    }
}

fn worker_main(id: usize, board: Arc<Scoreboard>, mode: Mode, rx: JobRx) {
    sb_set(id, board);
    loop {
        unsafe {
            // https://github.com/php/php-src/pull/9104
            // NTS inits module and request on different threads, so the call stack must be
            // re-initialized here.
            rapira_init_call_stack();
        };
        let exit: WorkerExit = match &mode {
            Mode::Classic => {
                classic_worker(rx.clone());
                WorkerExit::Closed
            }
            Mode::Worker(script) => rapira_worker(script.clone(), rx.clone()),
        };
        if matches!(exit, WorkerExit::Closed) {
            break;
        }
        // Restart: loop back on this same OS thread — rapira_init_call_stack runs
        // again and the worker re-bootstraps with a fresh request cycle.
    }
}

/// Block for the next job (shutdown-aware): `None` means the intake channel
/// closed — every `Sender`/`RapiraHandle` was dropped, i.e. Rapira is shutting
/// down. The single place the shared receiver is consumed; the classic loop,
/// worker-mode `next_job`, and the boot-failure drain all go through here.
pub(crate) fn pull_job(rx: &JobRx) -> Option<Job> {
    // A poisoned lock is a previous panic, not a closed channel — recover the
    // receiver so worker exit stays tied to channel closure.
    let mut guard = rx.lock().unwrap_or_else(|poisoned| {
        error!(target: "rapira", "worker channel lock poisoned; recovering");
        poisoned.into_inner()
    });
    guard.blocking_recv()
}
