use log::error;

use crate::{callbacks::send_error_head, *};
use std::{cell::RefCell, os::raw::c_char, path::PathBuf};

use crate::{
    TRACK_VARS_FILES,
    boot::JobRx,
    callbacks::guard,
    context::{bind_server_context, ctx, populate_request_context, unbind_server_context},
    executor::run_script,
    php_output_activate, php_output_end_all, php_request_shutdown, php_request_startup, rapira_eg,
    rapira_pg, rapira_run_handler, sapi_activate,
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
        if php_request_startup() == SUCCESS {
            run_script(&script); // blocks in while(rapira_handle_request)) in PHP
        }
    }

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
    unsafe {
        populate_request_context(&mut job.ctx);
        php_output_activate();
        sapi_activate();
        reset_super_globals();
        rapira_activate_auto_globals();
        let h_outcome: types::Outcome = rapira_run_handler(fci, fcc);
        if matches!(h_outcome, types::Outcome::Bailout | types::Outcome::Throw) {
            send_error_head(&job.ctx, 500);
        }
        let t_outcome: types::Outcome = rapira_request_teardown();
        if matches!(t_outcome, types::Outcome::Bailout) {
            error!(
                "[rapira] rapira_request_teardown() failed on first call {},{}",
                job.ctx.req.method, job.ctx.req.uri
            );
        }
    }

    log_and_clear_last_error();
    unbind_server_context();
    job.ctx.finish();
    true
}

fn next_job() -> Option<Job> {
    WORKER.with_borrow_mut(|w: &mut Option<WorkerChan>| {
        let wc = w.as_mut()?;
        if std::mem::take(&mut wc.first_call) {
            unsafe {
                let outcome: types::Outcome = rapira_request_teardown();
                if matches!(outcome, types::Outcome::Bailout) {
                    error!("[rapira] rapira_request_teardown() failed on first call");
                }
            }
        }

        log_and_clear_last_error();
        // TODO: no unwrap, handle error
        wc.rx.lock().unwrap().blocking_recv()
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
            php_output_end_all();
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
        if !(*rapira_pg()).last_error_message.is_null() {
            let msg = std::ffi::CStr::from_ptr((*rapira_pg()).last_error_message as *const c_char);
            error!(
                "[rapira] rapira_request_teardown() failed on first call: {}",
                msg.to_string_lossy()
            );
            // null out the last error message
            rapira_clear_last_error();
        }
    }
}
