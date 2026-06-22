use std::thread::JoinHandle;
use std::{os::raw::c_int, thread};
use tokio::sync::mpsc;
use types::Job;

use crate::{classic_worker::classic_worker, types::Mode, *};

const INTAKE_CAP: usize = 1024;

struct PhpThread;

impl PhpThread {
    pub(crate) fn new() -> Self {
        unsafe {
            ts_resource_ex(0, std::ptr::null_mut());
        }

        Self
    }
}

impl Drop for PhpThread {
    fn drop(&mut self) {
        unsafe {
            ts_free_thread();
        }
    }
}

pub struct Rapira {
    pub intake: tokio::sync::mpsc::Sender<Job>, // producer side
    dispatcher: JoinHandle<()>,                 // os thread: routes jobs
    workers: Vec<JoinHandle<()>>,               // os threads: execute PHP scripts
}

impl Rapira {
    pub fn boot(mode: Mode, req_threads: usize) -> anyhow::Result<Self> {
        let num_threads = req_threads.max(1);
        let mut module = module::build_sapi_module();
        let started = unsafe {
            php_tsrm_startup_ex(num_threads as c_int);
            sapi_startup(&mut module);
            module
                .startup
                .is_some_and(|start| start(&mut module) == ZEND_RESULT_CODE_SUCCESS)
        };

        if !started {
            unsafe {
                sapi_shutdown();
                tsrm_shutdown();
            }

            return Err(anyhow::anyhow!("php_module_startup failed"));
        }

        let (intake, intake_rx) = mpsc::channel::<Job>(INTAKE_CAP);
        let (idle_tx, idle_rx) = mpsc::unbounded_channel::<usize>();
        let mut inboxes: Vec<mpsc::Sender<Job>> = Vec::with_capacity(num_threads);

        let workers: Vec<JoinHandle<()>> = (0..num_threads)
            .map(|id: usize| {
                let (inbox_tx, inbox_rx) = mpsc::channel::<Job>(1);
                inboxes.push(inbox_tx);
                let (idle, m) = (idle_tx.clone(), mode.clone());
                thread::spawn(move || worker_main(id, m, inbox_rx, idle))
            })
            .collect();
        drop(idle_tx);

        let dispatcher: JoinHandle<()> =
            thread::spawn(move || dispatcher_loop(intake_rx, idle_rx, inboxes));

        Ok(Self {
            intake,
            dispatcher,
            workers,
        })
    }

    pub fn shutdown(self) {
        drop(self.intake); // intake closes -> dispatcher loop ends
        let _ = self.dispatcher.join(); // workers exits
        for w in self.workers {
            let _ = w.join();
        }
        unsafe {
            php_module_shutdown();
            sapi_shutdown();
            tsrm_shutdown();
        }
    }
}

fn dispatcher_loop(
    mut intake: mpsc::Receiver<Job>,
    mut idle: mpsc::UnboundedReceiver<usize>,
    inboxes: Vec<mpsc::Sender<Job>>,
) {
    while let Some(job) = intake.blocking_recv() {
        let Some(w) = idle.blocking_recv() else {
            break;
        };
        let _ = inboxes[w].blocking_send(job);
    }
}
fn worker_main(
    id: usize,
    mode: Mode,
    inbox: mpsc::Receiver<Job>,
    idle: mpsc::UnboundedSender<usize>,
) {
    let _php = PhpThread::new();
    match mode {
        Mode::Classic => classic_worker(id, inbox, idle),
        Mode::Worker(script) => rapira_worker::rapira_worker(id, script, inbox, idle),
    }
}
