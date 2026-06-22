use std::{cell::RefCell, path::PathBuf};

use tokio::sync::mpsc;

use crate::{
    TRACK_VARS_FILES, ZEND_RESULT_CODE_SUCCESS,
    callbacks::guard,
    context::{bind_server_context, ctx, populate_request_context, unbind_server_context},
    executor::run_script,
    php_output_activate, php_output_deactivate, php_output_end_all, php_request_shutdown,
    php_request_startup, rapira_eg, rapira_pg, rapira_run_handler, sapi_activate, sapi_deactivate,
    types::Job,
    zend_activate_auto_globals, zend_fcall_info, zend_fcall_info_cache, zend_hash_str_del,
    zval_ptr_dtor,
};

thread_local! {
    static WORKER: RefCell<Option<WorkerChan>> = const { RefCell::new(None) };
}

struct WorkerChan {
    id: usize,
    inbox: mpsc::Receiver<Job>,
    idle: mpsc::UnboundedSender<usize>,
    first_call: bool,
    _wc_served: usize, // TODO: not used - use
}

pub fn rapira_worker(
    id: usize,
    script: PathBuf,
    inbox: mpsc::Receiver<Job>,
    idle: mpsc::UnboundedSender<usize>,
) {
    WORKER.with(|w| {
        *w.borrow_mut() = Some(WorkerChan {
            id,
            inbox,
            idle,
            first_call: true,
            _wc_served: 0,
        })
    });

    unsafe {
        if php_request_startup() == ZEND_RESULT_CODE_SUCCESS {
            run_script(&script); // blocks in while(rapira_handle_request)) in PHP
        }

        if WORKER.with(|w| w.borrow().as_ref().is_some_and(|wc| wc.first_call)) {
            php_request_shutdown(std::ptr::null_mut());
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_handle_request(
    // Safety: not safe
    fci: *mut zend_fcall_info,
    fcc: *mut zend_fcall_info_cache,
) -> bool {
    guard(false, || {
        let next = WORKER.with(|w| {
            let mut b = w.borrow_mut();
            let wc: &mut WorkerChan = b.as_mut()?;
            if std::mem::replace(&mut wc.first_call, false) {
                unsafe {
                    php_output_end_all();
                    sapi_deactivate();
                }
            }
            wc.idle.send(wc.id).ok()?;
            wc.inbox.blocking_recv()
        });

        let mut job: Job = match next {
            Some(j) => j,
            None => return false,
        };

        bind_server_context(&mut job.ctx);
        unsafe {
            populate_request_context(&mut job.ctx);
            php_output_activate();
            sapi_activate();
            reset_super_globals();
            zend_activate_auto_globals();
        }
        unsafe {
            // main.c:1967
            // if ZEND_OBSERVER_ENABLED {
            // zend_observer_fcall_begin(handler);
            // }
            // call_php_zval(handler);
            // if (*rapira_pg()).modules_activated {
            //     // php_call_shutdown_functions();
            // }
            // zend_call_destructors();

            let outcome = rapira_run_handler(fci, fcc);
            let is_err = matches!(outcome, 1 /* BAILOUT */ | 3 /* THROW */);
            if is_err
                && !job.ctx.headers_sent
                && let Some(tx) = &job.ctx.tx
            {
                let _ = tx.blocking_send(crate::types::Frame::Head(crate::types::ResponseHead {
                    status: 500,
                    headers: vec![],
                }));
            }
            php_output_end_all();
            php_output_deactivate();
            sapi_deactivate();
        }
        unbind_server_context();
        job.ctx.finish();
        true
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_finish_request() {
    guard((), || {
        unsafe {
            php_output_end_all();
        }
        if let Some(c) = ctx() {
            c.finish();
        }
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
