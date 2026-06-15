use crate::types::{Context, Frame};
use crate::*;
use bytes::Bytes;
use std::os::raw::{c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};

pub(crate) unsafe extern "C" fn sapi_startup_cb(sapi_module: *mut sapi_module_struct) -> c_int {
    // https://doc.rust-lang.org/reference/expressions/operator-expr.html#raw-borrow-operators
    unsafe { php_module_startup(sapi_module, &raw mut rapira_module_entry) }
}

pub(crate) unsafe extern "C" fn sapi_shutdown_cb(sapi_module: *mut sapi_module_struct) -> c_int {
    unsafe {
        php_module_shutdown();
    }
    ZEND_RESULT_CODE_SUCCESS
}

pub(crate) unsafe extern "C" fn sapi_deactivate_cb() -> c_int {
    ZEND_RESULT_CODE_SUCCESS
}

fn ctx<'a>() -> Option<&'a mut Context> {
    unsafe { ((*rapira_sg()).server_context as *mut Context).as_mut() }
}

fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(default)
}

pub(crate) unsafe extern "C" fn ub_write(buf: *const c_char, len: usize) -> usize {
    guard(0, || {
        let Some(c) = ctx() else { return len };
        if !c.headers_sent {
            // send_headers
        }

        if let Some(tx) = &c.tx {
            let buf = unsafe { std::slice::from_raw_parts(buf as *const u8, len) };
            let _ = tx.send(Frame::Body(Bytes::copy_from_slice(buf)));
        }

        len
    })
}
