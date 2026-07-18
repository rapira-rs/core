use std::{fs::File, io::ErrorKind};

use crate::{
    callbacks::{finalize_response, send_error_head},
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
        let (event, truncated) = classic_executor(&mut job);
        sb_update(event);
        job.ctx.finish(truncated);
    }
}

fn classic_executor(job: &mut Job) -> (Event, bool) {
    bind_server_context(&mut job.ctx);
    let (is_errored, truncated) = unsafe {
        populate_request_context(&mut job.ctx);
        if php_request_startup() == FAILURE {
            send_error_head(&mut job.ctx, 500);
            rapira_request_shutdown();
            unbind_server_context();
            return (Event::Handled(true), false);
        }

        let exec_err: bool = match File::open(&job.ctx.req.script_filename) {
            Err(e) => {
                send_error_head(&mut job.ctx, status_for_open_error(e.kind()));
                true
            }
            Ok(_) => {
                // sb_update counts an error as errored = true, but run_script returns
                // false to signal a PHP error — reverse it to match that expectation
                !run_script(&job.ctx.req.script_filename)
            }
        };
        // The script has run: from here the flush is teardown, not streaming, so
        // freeze `stream` — a buffered body flushed now stays a complete response.
        job.ctx.tearing_down = true;
        // flushes output and sends the REAL head (script status + Set-Cookie) via
        // php_output_deactivate -> sapi_send_headers
        rapira_request_shutdown();

        // truncated only if the body was already streaming when the script failed; a 500 is
        // synthesized only when nothing emitted a head (never pre-empts the real head)
        let truncated = finalize_response(&mut job.ctx, exec_err);

        (exec_err, truncated)
    };

    unbind_server_context();
    (Event::Handled(is_errored), truncated)
}
