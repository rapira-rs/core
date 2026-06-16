use std::path::PathBuf;

use crate::{
    context::{bind_server_context, populate_request_context},
    types::Job,
    *,
};
use tokio::sync::mpsc;
pub(crate) fn classic_worker(
    id: usize,
    mut inbox: mpsc::Receiver<Job>,
    idle: mpsc::UnboundedSender<usize>,
) {
    loop {
        if idle.send(id).is_err() {
            // todo: log error
            break;
        }

        let Some(mut job) = inbox.blocking_recv() else {
            break;
        };

        classic_executor(&mut job);
    }
}

fn classic_executor(job: &mut Job) {
    bind_server_context(&mut job.ctx);
    unsafe {
        populate_request_context(&mut job.ctx);
        if php_request_startup() == ZEND_RESULT_CODE_FAILURE {}
    }
}

pub(crate) fn wait_worker(
    id: usize,
    script: PathBuf,
    mut inbox: mpsc::Receiver<Job>,
    idle: mpsc::UnboundedSender<usize>,
) {
}
