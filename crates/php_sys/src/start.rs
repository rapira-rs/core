use log::{error, info, trace};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use tokio::sync::mpsc::{self, Receiver, Sender};
use types::Job;

#[cfg(not(php_zts))]
use log::warn;

#[cfg(php_zts)]
use std::os::raw::c_int;
#[cfg(php_zts)]
use std::ptr::null_mut;

use crate::rapira_worker::{WorkerExit, rapira_worker};
use crate::scoreboard::{Scoreboard, ScoreboardSnapshot, sb_set};
use crate::{classic_worker::classic_worker, types::Mode, *};

pub(crate) type JobRx = Arc<Mutex<Receiver<Job>>>;

const INTAKE_CAP: usize = 1024;

struct PhpThread;

impl PhpThread {
    pub(crate) fn new() -> Self {
        #[cfg(php_zts)]
        unsafe {
            ts_resource_ex(0, null_mut());
        }
        Self
    }
}

impl Drop for PhpThread {
    fn drop(&mut self) {
        #[cfg(php_zts)]
        unsafe {
            ts_free_thread();
        }
    }
}

pub struct Rapira {
    pub(crate) intake: Option<Sender<Job>>, // producer side
    workers: Vec<JoinHandle<()>>,           // os threads: execute PHP scripts
    pub scoreboard: Arc<Scoreboard>,        // shared scoreboard for workers
    _not_send: PhantomData<*const ()>, // !Send + !Sync, to prevent dropping from a foreign thread (which would be UB)
}

impl Rapira {
    pub fn start(mode: Mode, req_threads: usize) -> anyhow::Result<Self> {
        info!("[rapira] booting with mode: {mode:?}, threads: {req_threads}");
        // NTS: 1 thread only
        #[cfg(not(php_zts))]
        let num_threads: usize = {
            if req_threads > 1 {
                warn!("[rapira] ZTS not enabled, only 1 thread will be used");
            }
            1
        };

        #[cfg(php_zts)]
        let num_threads: usize = req_threads.max(1);
        let scoreboard: Arc<Scoreboard> = Scoreboard::new(num_threads);

        let mut module: _sapi_module_struct = module::build_sapi_module();
        let started: bool = unsafe {
            #[cfg(php_zts)]
            php_tsrm_startup_ex(num_threads as c_int);
            rapira_process_init();
            sapi_startup(&mut module);
            module
                .startup
                .is_some_and(|start| start(&mut module) == SUCCESS)
        };

        if !started {
            error!("[rapira] php_module_startup failed, shutting down");
            unsafe {
                php_module_shutdown();
                sapi_shutdown();
                #[cfg(php_zts)]
                tsrm_shutdown();
            }

            return Err(anyhow::anyhow!("php_module_startup failed"));
        }

        let (intake, intake_rx) = mpsc::channel::<Job>(INTAKE_CAP);
        let rx: JobRx = Arc::new(Mutex::new(intake_rx));

        let workers: Vec<JoinHandle<()>> = (0..num_threads)
            .map(|id| {
                let (rx, mode, scoreboard) = (rx.clone(), mode.clone(), scoreboard.clone());
                trace!("[rapira] spawning worker thread");
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
        info!("[rapira] shutdown is noop, deinitialize in Drop");
    }

    pub fn scoreboard(&self) -> ScoreboardSnapshot {
        self.scoreboard.snapshot()
    }
}

impl Drop for Rapira {
    fn drop(&mut self) {
        info!("[rapira] shutting down, dropping");
        self.intake = None;
        for w in std::mem::take(&mut self.workers) {
            let _ = w.join();
        }
        unsafe {
            php_module_shutdown();
            sapi_shutdown();
            #[cfg(php_zts)]
            tsrm_shutdown();
        }
    }
}

fn worker_main(id: usize, board: Arc<Scoreboard>, mode: Mode, rx: JobRx) {
    sb_set(id, board);
    loop {
        let php = PhpThread::new();
        #[cfg(not(php_zts))]
        unsafe {
            // https://github.com/php/php-src/pull/9104
            // in NTS, we init module and request on the different threads
            // so we have to init call stack again
            rapira_init_call_stack();
        };
        let exit: WorkerExit = match &mode {
            Mode::Classic => {
                classic_worker(rx.clone());
                WorkerExit::Closed
            }
            Mode::Worker(script) => rapira_worker(script.clone(), rx.clone()),
        };
        drop(php); // ZTS: ts_free_thread — globals dtor'd, TLS cache cleared
        if matches!(exit, WorkerExit::Closed) {
            break;
        }
        // Restart: the next PhpThread::new() re-runs ts_resource on this same
        // OS thread — fresh per-thread globals, ctors incl. zend_call_stack_init
    }
}

/// Block for the next job (shutdown-aware): `None` means the intake channel
/// closed — every `Sender`/`RapiraHandle` was dropped, i.e. Rapira is shutting
/// down. The single place the shared receiver is consumed; the classic loop,
/// worker-mode `next_job`, and the boot-failure drain all go through here.
pub(crate) fn pull_job(rx: &JobRx) -> Option<Job> {
    match rx.lock() {
        Ok(mut guard) => guard.blocking_recv(),
        Err(err) => {
            error!("[rapira] pull_job() failed to lock worker channel: {err}");
            None
        }
    }
}
