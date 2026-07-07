use std::{fs::File, io::ErrorKind};

use crate::{
    callbacks::send_error_head,
    context::{bind_server_context, populate_request_context, unbind_server_context},
    executor::run_script,
    scoreboard::{Event, sb_update},
    start::{JobRx, pull_job},
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
    while let Some(mut job) = pull_job(&rx) {
        sb_update(classic_executor(&mut job));
        job.ctx.finish();
    }
}

fn classic_executor(job: &mut Job) -> Event {
    bind_server_context(&mut job.ctx);
    let is_errored: bool = unsafe {
        populate_request_context(&mut job.ctx);
        if php_request_startup() == FAILURE {
            send_error_head(&mut job.ctx, 500);
            rapira_request_shutdown();
            unbind_server_context();
            return Event::Handled(true);
        }

        let exec_err: bool = match File::open(&job.ctx.req.script_filename) {
            Err(e) => {
                send_error_head(&mut job.ctx, status_for_open_error(e.kind()));
                true
            }
            Ok(_) => {
                // in the sb_record we count errors as errored = true
                // here run_script may return false, as an indicator of a PHP error
                // so we need to reverse the logic to match the sb_record expectation
                !run_script(&job.ctx.req.script_filename)
            }
        };
        // flushes output and sends the REAL head (script status + Set-Cookie) via
        // php_output_deactivate -> sapi_send_headers
        rapira_request_shutdown();

        // fallback ONLY if nothing emitted a head (script bailed before any output
        // and the flush sent none) - never pre-empts the real head now
        if exec_err && !job.ctx.headers_sent {
            send_error_head(&mut job.ctx, 500);
        }

        exec_err
    };

    unbind_server_context();
    Event::Handled(is_errored)
}
