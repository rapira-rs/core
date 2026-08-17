use std::{fs::File, io::ErrorKind};

use crate::{
    callbacks::{finalize_response, send_error_head},
    context::{bind_server_context, populate_request_context, unbind_server_context},
    executor::run_script,
    scoreboard::{Event, sb_update},
    start::pull_job,
    types::Job,
    *,
};

// map a script-open failure to an HTTP status: missing -> 404, unreadable -> 403, else 500
fn status_for_open_error(kind: ErrorKind) -> u16 {
    match kind {
        ErrorKind::NotFound => 404,
        ErrorKind::PermissionDenied => 403,
        _ => 500,
    }
}

pub(crate) fn classic_worker() {
    while let Some(mut job) = pull_job() {
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
        crate::context::apply_proto_num(&job.ctx);

        let exec_err: bool = match File::open(&job.ctx.req.script_filename) {
            Err(e) => {
                send_error_head(&mut job.ctx, status_for_open_error(e.kind()));
                true
            }
            Ok(_) => {
                // run_script is also false for exit()/die(), which unwind without an
                // error. A real failure leaves a bailout (unclean_shutdown) or a
                // recorded fatal: the "Uncaught ..." path goes through php_error_cb
                // with E_DONT_BAIL, so only last_error tells it apart from exit().
                // last_error_type survives php_free_request_globals; the message
                // pointer is the per-request freshness gate.
                let failed = !run_script(&job.ctx.req.script_filename);
                let pg = rapira_pg();
                failed
                    && ((*rapira_cg()).unclean_shutdown
                        || (!(*pg).last_error_message.is_null()
                            && (*pg).last_error_type & E_FATAL_ERRORS as i32 != 0))
            }
        };
        // The script has run: from here the flush is teardown, not streaming, so
        // freeze `stream` - a buffered body flushed now stays a complete response.
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
