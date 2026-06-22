use crate::{
    context::{bind_server_context, populate_request_context, unbind_server_context},
    executor::run_script,
    types::{Frame, Job, ResponseHead},
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
        if php_request_startup() == ZEND_RESULT_CODE_FAILURE {
            if let Some(tx) = &job.ctx.tx {
                let _ = tx.blocking_send(Frame::Head(ResponseHead {
                    status: 500,
                    headers: vec![],
                }));
            }
            php_request_shutdown(std::ptr::null_mut());
            unbind_server_context();
            job.ctx.finish();
            return;
        }
        run_script(&job.ctx.req.script_filename);
        php_request_shutdown(std::ptr::null_mut());
    }
    unbind_server_context();
    job.ctx.finish();
}
