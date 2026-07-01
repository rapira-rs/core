use log::error;

use crate::{callbacks::send_error_head, scoreboard::sb_record, *};
use std::{cell::RefCell, path::PathBuf};

use crate::{
    TRACK_VARS_FILES,
    callbacks::guard,
    context::{bind_server_context, ctx, populate_request_context, unbind_server_context},
    executor::run_script,
    php_output_activate, php_request_shutdown, php_request_startup, rapira_eg, rapira_pg,
    rapira_run_handler, sapi_activate,
    start::JobRx,
    types::Job,
    zend_fcall_info, zend_fcall_info_cache, zend_hash_str_del, zval_ptr_dtor,
};

thread_local! {
    static WORKER: RefCell<Option<WorkerChan>> = const { RefCell::new(None) };
}

struct WorkerChan {
    rx: JobRx,
    first_call: bool,
}

pub fn rapira_worker(script: PathBuf, rx: JobRx) {
    WORKER.with_borrow_mut(|w: &mut Option<WorkerChan>| {
        *w = Some(WorkerChan {
            rx,
            first_call: true,
        })
    });

    unsafe {
        match php_request_startup() {
            FAILURE => {
                error!("[rapira] rapira_worker() failed to start request");
                sb_record(true);
            }
            SUCCESS => {
                // all should fail if not ok
                // TODO: startup algorithm should be more robust
                let ok: bool = run_script(&script); // blocks in while(rapira_handle_request)) in PHP
                if !ok {
                    error!(
                        "[rapira] rapira_worker() failed to run script: {:?}",
                        script
                    );
                }
            }
            _ => {
                error!("[rapira] rapira_worker() unexpected php_request_startup() result");
                sb_record(true);
            }
        }
    };

    if WORKER.with_borrow(|w: &Option<WorkerChan>| {
        w.as_ref().is_some_and(|wc: &WorkerChan| wc.first_call)
    }) {
        unsafe {
            php_request_shutdown(std::ptr::null_mut());
        }
    }
}

/// # Safety
/// Invoked from C (the `rapira_handle_request` PHP function) once per worker-loop
/// iteration. `fci` and `fcc` must be valid, non-null pointers produced by
/// `Z_PARAM_FUNC` and remain valid for the call. Must run on the resident worker
/// thread whose `WORKER` thread-local is initialized, inside its active request.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_handle_request(
    // Safety: not safe
    fci: *mut zend_fcall_info,
    fcc: *mut zend_fcall_info_cache,
) -> bool {
    guard(false, || handle_request_impl(fci, fcc))
}

fn handle_request_impl(fci: *mut zend_fcall_info, fcc: *mut zend_fcall_info_cache) -> bool {
    let Some(mut job) = next_job() else {
        return false;
    };

    bind_server_context(&mut job.ctx);
    let errored = unsafe {
        let mut err = false;
        populate_request_context(&mut job.ctx);
        php_output_activate();
        sapi_activate();
        reset_super_globals();
        rapira_activate_auto_globals();
        let h_outcome: types::Outcome = rapira_run_handler(fci, fcc);
        if matches!(h_outcome, types::Outcome::Bailout | types::Outcome::Throw) {
            send_error_head(&job.ctx, 500);
            err = true;
        }
        let t_outcome: types::Outcome = rapira_request_teardown();
        if matches!(t_outcome, types::Outcome::Bailout) {
            error!(
                "[rapira] rapira_request_teardown() bailed {},{}",
                job.ctx.req.method, job.ctx.req.uri
            );
            err = true;
        }
        err
    };

    log_and_clear_last_error();
    unbind_server_context();
    sb_record(errored);
    job.ctx.finish();
    true
}

fn next_job() -> Option<Job> {
    WORKER.with_borrow_mut(|w: &mut Option<WorkerChan>| {
        let wc = w.as_mut()?;
        // first iteration: clean up whatever php_request_startup()'s bootstrap
        // left before serving real requests — there's no prior request yet
        if std::mem::take(&mut wc.first_call) {
            unsafe {
                let outcome: types::Outcome = rapira_request_teardown();
                if matches!(outcome, types::Outcome::Bailout) {
                    error!("[rapira] rapira_request_teardown() failed on first call");
                }
            }
        }

        log_and_clear_last_error();
        match wc.rx.lock() {
            Ok(mut job) => job.blocking_recv(),
            Err(err) => {
                error!("[rapira] next_job() failed to lock worker channel: {err}");
                None
            }
        }
    })
}

/// # Safety
/// Invoked from C (the `rapira_finish_request` PHP function). Must run on a worker
/// thread inside an active request whose `Context` is bound in `SG(server_context)`;
/// it flushes output and finishes the response stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_finish_request() {
    guard((), || {
        unsafe {
            let outcome = rapira_finish_output();
            if matches!(outcome, types::Outcome::Bailout) {
                log::error!("[rapira] rapira_finish_output() bailed");
            }
        }
        unsafe {
            if let Some(c) = ctx() {
                c.finish();
            }
        };
    })
}

unsafe fn reset_super_globals() {
    let files: &mut crate::_zval_struct =
        unsafe { &mut (*rapira_pg()).http_globals[TRACK_VARS_FILES as usize] };
    unsafe {
        zval_ptr_dtor(files);
        *files = std::mem::zeroed();
        let _ = zend_hash_str_del(&mut (*rapira_eg()).symbol_table, c"_SESSION".as_ptr(), 8);
    }
}

fn log_and_clear_last_error() {
    unsafe {
        let zend_str = (*rapira_pg()).last_error_message;
        if zend_str.is_null() {
            return;
        }
        let msg =
            std::slice::from_raw_parts((*zend_str).val.as_ptr().cast::<u8>(), (*zend_str).len);
        error!("[rapira] last PHP error: {}", String::from_utf8_lossy(msg));
        // null out the last error message
        rapira_clear_last_error();
    }
}
