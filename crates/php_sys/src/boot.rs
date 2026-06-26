use log::{error, info, trace};
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

use crate::rapira_worker::rapira_worker;
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
}

impl Rapira {
    pub fn boot(mode: Mode, req_threads: usize) -> anyhow::Result<Self> {
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

        let mut module: _sapi_module_struct = module::build_sapi_module();
        let started: bool = unsafe {
            #[cfg(php_zts)]
            php_tsrm_startup_ex(num_threads as c_int);
            sapi_startup(&mut module);
            module
                .startup
                .is_some_and(|start| start(&mut module) == SUCCESS)
        };

        if !started {
            error!("[rapira] php_module_startup failed, shutting down");
            unsafe {
                sapi_shutdown();
                #[cfg(php_zts)]
                tsrm_shutdown();
            }

            return Err(anyhow::anyhow!("php_module_startup failed"));
        }

        let (intake, intake_rx) = mpsc::channel::<Job>(INTAKE_CAP);
        let rx: JobRx = Arc::new(Mutex::new(intake_rx));

        let workers: Vec<JoinHandle<()>> = (0..num_threads)
            .map(|_| {
                let (rx, mode) = (rx.clone(), mode.clone());
                trace!("[rapira] spawning worker thread");
                thread::spawn(move || worker_main(mode, rx))
            })
            .collect();

        Ok(Self {
            intake: Some(intake),
            workers,
        })
    }

    pub fn shutdown(self) {
        info!("[rapira] shutdown in noop, deinitialize in Drop");
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

fn worker_main(mode: Mode, rx: JobRx) {
    let _php = PhpThread::new();
    #[cfg(not(php_zts))]
    unsafe {
        // https://github.com/php/php-src/pull/9104
        // in NTS, we init module and request on the different threads
        // so we have to init call stack again
        rapira_init_call_stack();
    };
    match mode {
        Mode::Classic => classic_worker(rx),
        Mode::Worker(script) => rapira_worker(script, rx),
    }
}
