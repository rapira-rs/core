use std::{
    ffi::{CStr, c_char, c_int},
    ptr::null_mut,
    slice::from_raw_parts,
};

use tracing::event;

use crate::callbacks::guard;

/// # Safety
/// `len` must be a writable `usize`. The returned pointer is `'static`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_version(len: *mut usize) -> *const c_char {
    const VERSION: &CStr =
        match CStr::from_bytes_with_nul(concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes()) {
            Ok(v) => v,
            Err(_) => c"unknown",
        };

    guard(null_mut(), || {
        unsafe {
            len.write(VERSION.count_bytes());
        };
        VERSION.as_ptr()
    })
}

/// # Safety
/// `msg` must point at `msg_len` readable bytes; `ctx` at `ctx_len` readable
/// bytes, or be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_log(
    msg: *const c_char,
    msg_len: usize,
    level: c_int,
    ctx: *const c_char,
    ctx_len: usize,
) {
    // event! dispatches into the installed subscriber, which is arbitrary code;
    // an unwind out of an extern "C" frame aborts the process.
    guard((), || {
        let message = String::from_utf8_lossy(unsafe { from_raw_parts(msg.cast::<u8>(), msg_len) });
        let context = if ctx.is_null() {
            std::borrow::Cow::Borrowed("")
        } else {
            String::from_utf8_lossy(unsafe { from_raw_parts(ctx.cast::<u8>(), ctx_len) })
        };

        // to macros
        match level {
            0 => {
                if context.is_empty() {
                    event!(target: "app", tracing::Level::ERROR, "{message}");
                } else {
                    event!(target: "app", tracing::Level::ERROR, context=%context, "{message}");
                }
            }
            1 => {
                if context.is_empty() {
                    event!(target: "app", tracing::Level::WARN, "{message}");
                } else {
                    event!(target: "app", tracing::Level::WARN, context=%context, "{message}");
                }
            }
            2 => {
                if context.is_empty() {
                    event!(target: "app", tracing::Level::INFO, "{message}");
                } else {
                    event!(target: "app", tracing::Level::INFO, context=%context, "{message}");
                }
            }
            3 => {
                if context.is_empty() {
                    event!(target: "app", tracing::Level::DEBUG, "{message}");
                } else {
                    event!(target: "app", tracing::Level::DEBUG, context=%context, "{message}");
                }
            }
            4 => {
                if context.is_empty() {
                    event!(target: "app", tracing::Level::TRACE, "{message}");
                } else {
                    event!(target: "app", tracing::Level::TRACE, context=%context, "{message}");
                }
            }
            _ => {
                if context.is_empty() {
                    event!(target: "app", tracing::Level::INFO, "{message}");
                } else {
                    event!(target: "app", tracing::Level::INFO, context=%context, "{message}");
                }
            }
        }
    })
}
