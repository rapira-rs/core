use std::{fs::File, io::ErrorKind, ptr::null_mut};

use crate::{
    boot::JobRx,
    callbacks::send_error_head,
    context::{bind_server_context, populate_request_context, unbind_server_context},
    executor::run_script,
    scoreboard::sb_record,
    types::Job,
    *,
};

// cgi/fmp like
fn status_for_open_error(kind: ErrorKind) -> u16 {
    match kind {
        ErrorKind::NotFound => 404,
        ErrorKind::PermissionDenied => 403,
        _ => 500,
    }
}

pub(crate) fn classic_worker(rx: JobRx) {
    loop {
        // TODO: boom, unwrap
        let job: Option<Job> = rx.lock().unwrap().blocking_recv();
        let Some(mut job) = job else { break };

        sb_record(classic_executor(&mut job));
        job.ctx.finish();
    }
}

fn classic_executor(job: &mut Job) -> bool {
    bind_server_context(&mut job.ctx);
    let is_errored: bool = unsafe {
        populate_request_context(&mut job.ctx);
        if php_request_startup() == FAILURE {
            send_error_head(&job.ctx, 500);
            php_request_shutdown(null_mut());
            unbind_server_context();
            return true;
        }

        let exec_err: bool = match File::open(&job.ctx.req.script_filename) {
            Err(e) => {
                send_error_head(&job.ctx, status_for_open_error(e.kind()));
                true
            }
            Ok(_) => {
                let ok: bool = run_script(&job.ctx.req.script_filename);
                if !ok && !job.ctx.headers_sent {
                    send_error_head(&job.ctx, 500);
                }
                // in the sb_record we count errors as errored = true
                // here run_script may return false, as an indicator of a PHP error
                // so we need to reverse the logic to match the sb_record expectation
                !ok
            }
        };

        php_request_shutdown(null_mut());
        exec_err
    };

    unbind_server_context();
    is_errored
}
