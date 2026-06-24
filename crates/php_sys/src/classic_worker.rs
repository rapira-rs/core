use crate::{
    boot::JobRx,
    callbacks::send_error_head,
    context::{bind_server_context, populate_request_context, unbind_server_context},
    executor::run_script,
    types::Job,
    *,
};
pub(crate) fn classic_worker(rx: JobRx) {
    loop {
        // TODO: boom, unwrap
        let job: Option<Job> = rx.lock().unwrap().blocking_recv();
        let Some(mut job) = job else { break };

        classic_executor(&mut job);
    }
}

fn classic_executor(job: &mut Job) {
    bind_server_context(&mut job.ctx);
    unsafe {
        populate_request_context(&mut job.ctx);
        if php_request_startup() == ZEND_RESULT_CODE_FAILURE {
            send_error_head(&job.ctx, 500);
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
