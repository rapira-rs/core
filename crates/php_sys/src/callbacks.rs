use crate::context::ctx;
use crate::types::{Context, Frame, ResponseHead};
use crate::*;
use bytes::Bytes;
use core::slice;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

pub fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(default)
}

struct SapiHeaders(*mut sapi_headers_struct);

impl SapiHeaders {
    fn status(&self) -> u16 {
        let h = unsafe { &*self.0 };
        if h.http_response_code != 0 {
            h.http_response_code as u16
        } else {
            200
        }
    }

    fn lines(&self) -> impl Iterator<Item = SapiHeader> {
        let mut el = unsafe { &mut *self.0 }.headers.head;
        std::iter::from_fn(move || {
            let e = unsafe { el.as_ref()? };
            el = e.next;
            Some(SapiHeader(e.data.as_ptr() as *const sapi_header_struct))
        })
    }
}

struct SapiHeader(*const sapi_header_struct);

impl SapiHeader {
    fn name_value(&self) -> Option<(String, String)> {
        let sh = unsafe { &*self.0 };
        if sh.header.is_null() || sh.header_len == 0 {
            return None;
        }

        let line = unsafe { slice::from_raw_parts(sh.header as *const u8, sh.header_len) };

        let i = line.iter().position(|&b| b == b':')?;
        let k = String::from_utf8_lossy(&line[..i]).trim().to_string();
        let v = String::from_utf8_lossy(&line[i + 1..]).trim().to_string();
        (!k.is_empty()).then_some((k, v))
    }
}

pub(crate) unsafe extern "C" fn sapi_startup_cb(sapi_module: *mut sapi_module_struct) -> c_int {
    // https://doc.rust-lang.org/reference/expressions/operator-expr.html#raw-borrow-operators
    unsafe { php_module_startup(sapi_module, &raw mut rapira_module_entry) }
}

pub(crate) unsafe extern "C" fn sapi_shutdown_cb(_sapi_module: *mut sapi_module_struct) -> c_int {
    unsafe {
        php_module_shutdown();
    }
    ZEND_RESULT_CODE_SUCCESS
}

pub(crate) unsafe extern "C" fn sapi_deactivate_cb() -> c_int {
    ZEND_RESULT_CODE_SUCCESS
}

pub(crate) unsafe extern "C" fn ub_write(buf: *const c_char, len: usize) -> usize {
    guard(0, || {
        let ctx = unsafe {
            let Some(c) = ctx() else { return len };
            c
        };

        if !ctx.headers_sent {
            send_head(ctx);
        }

        if let Some(tx) = &ctx.sender {
            let buf = unsafe { slice::from_raw_parts(buf as *const u8, len) };
            if tx
                .blocking_send(Frame::Body(Bytes::copy_from_slice(buf)))
                .is_err()
            {
                return 0;
            }
        }

        len
    })
}

/// # Safety
/// A SAPI callback invoked by PHP. `h` must be a valid `*mut sapi_headers_struct`
/// for the duration of the call (PHP guarantees this when firing the `send_headers`
/// hook). Must run on a worker thread inside an active request whose `Context` is
/// bound in `SG(server_context)`.
pub unsafe extern "C" fn send_headers(h: *mut sapi_headers_struct) -> c_int {
    guard(SAPI_HEADER_SEND_FAILED as c_int, || {
        let ctx = unsafe {
            let Some(ctx) = ctx() else {
                return SAPI_HEADER_SEND_FAILED as c_int;
            };
            ctx
        };

        let h = SapiHeaders(h);
        let headers: Vec<(String, String)> = h
            .lines()
            .filter_map(|l: SapiHeader| l.name_value())
            .collect();
        if let Some(tx) = &ctx.sender {
            let _ = tx.blocking_send(Frame::Head(ResponseHead {
                status: h.status(),
                headers,
            }));
        };
        ctx.headers_sent = true;
        SAPI_HEADER_SENT_SUCCESSFULLY as c_int
    })
}

pub(crate) unsafe extern "C" fn flush(_sc: *mut c_void) {
    // nothing to do, chunks already sent
}

pub(crate) unsafe extern "C" fn read_post(buf: *mut c_char, count: usize) -> usize {
    guard(0, || {
        let ctx = unsafe {
            let Some(ctx) = ctx() else { return 0 };
            ctx
        };

        let dst = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, count) };
        ctx.req.body.read(dst).unwrap_or(0)
    })
}

pub(crate) unsafe extern "C" fn read_cookies() -> *mut c_char {
    unsafe {
        guard(std::ptr::null_mut(), || match ctx() {
            Some(ctx) => {
                ctx.c.cookie.as_ptr() as *mut c_char // we own the buffer
            }
            None => std::ptr::null_mut(),
        })
    }
}

pub(crate) unsafe extern "C" fn register_server_variables(track_vars_array: *mut zval) {
    guard((), || {
        let ctx = unsafe {
            let Some(ctx) = ctx() else { return };
            ctx
        };
        let put = |name: &str, val: &str| unsafe {
            let n = CString::new(name).unwrap_or_default();
            let v = CString::new(val).unwrap_or_default();
            php_register_variable_safe(
                n.as_ptr(),
                v.as_ptr() as *const c_char,
                v.as_bytes().len(),
                track_vars_array,
            );
        };
        // mapping trust policy: https://www.php.net/manual/en/reserved.variables.server.php
        put("PHP_SELF", &ctx.req.script_name);
        let doc_uri = ctx
            .req
            .uri
            .split_once('?')
            .map_or(ctx.req.uri.as_str(), |(p, _)| p);
        put("DOCUMENT_URI", doc_uri);
        put("DOCUMENT_ROOT", &ctx.req.document_root);
        put(
            "REQUEST_SCHEME",
            if ctx.req.https { "https" } else { "http" },
        );
        put("REMOTE_HOST", &ctx.req.remote_addr);
        put("REMOTE_PORT", &ctx.req.remote_port);
        put("REMOTE_IDENT", ""); // RFC 1413: "The REMOTE_IDENT variable is not set by default"
        put("REQUEST_METHOD", &ctx.req.method);
        put("REQUEST_URI", &ctx.req.uri);
        put("QUERY_STRING", &ctx.req.query);
        put(
            "SCRIPT_FILENAME",
            &ctx.req.script_filename.to_string_lossy(),
        );
        put("SCRIPT_NAME", &ctx.req.script_name);
        put("SERVER_PROTOCOL", &ctx.req.protocol);
        put("SERVER_SOFTWARE", "Rapira");
        put("SERVER_NAME", &ctx.req.server_name);
        put("SERVER_PORT", &ctx.req.server_port);
        put("REMOTE_ADDR", &ctx.req.remote_addr);

        let auth_type = ctx
            .req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .and_then(|(_, v)| v.split_whitespace().next())
            .unwrap_or("");

        put("AUTH_TYPE", auth_type);
        let auth_user = unsafe { (*rapira_sg()).request_info.auth_user };
        if !auth_user.is_null() {
            let user = unsafe { std::ffi::CStr::from_ptr(auth_user as *const i8) };
            put("REMOTE_USER", &user.to_string_lossy());
        }

        if let Some(ct) = &ctx.req.content_type {
            put("CONTENT_TYPE", ct);
        }
        if ctx.req.content_length >= 0 {
            put("CONTENT_LENGTH", &ctx.req.content_length.to_string());
        }

        for (k, v) in &ctx.req.headers {
            put(
                &format!("HTTP_{}", k.to_ascii_uppercase().replace("-", "_")),
                v,
            );
        }
        for (k, v) in &ctx.req.server_vars {
            put(k, v);
        }
    })
}

pub(crate) unsafe extern "C" fn getenv_cb(_n: *const c_char, _l: usize) -> *mut c_char {
    std::ptr::null_mut()
}

pub(crate) unsafe extern "C" fn log_message(message: *const c_char, _message_len: c_int) {
    // TODO: double check with php-src/other impl for the correct way to log messages from PHP
    let s: String = unsafe { std::ffi::CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned();
    eprintln!("[php] {s}");
}

fn send_head(c: &mut Context) {
    if c.headers_sent {
        return;
    }
    let status = unsafe { SapiHeaders(&mut (*rapira_sg()).sapi_headers).status() };

    if let Some(tx) = &c.sender {
        let _ = tx.blocking_send(Frame::Head(ResponseHead {
            status,
            headers: vec![],
        }));
        c.headers_sent = true;
    }
}

pub(crate) fn send_error_head(c: &Context, status: u16) {
    if c.headers_sent {
        return;
    }

    if let Some(tx) = &c.sender {
        let _ = tx.blocking_send(Frame::Head(ResponseHead {
            status,
            headers: vec![],
        }));
    }
}
