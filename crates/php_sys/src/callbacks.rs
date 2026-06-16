use crate::types::{Context, Frame, ResponseHead};
use crate::*;
use bytes::Bytes;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

fn ctx<'a>() -> Option<&'a mut Context> {
    unsafe { ((*rapira_sg()).server_context as *mut Context).as_mut() }
}

fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(default)
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
        let Some(c) = ctx() else { return len };
        if !c.headers_sent {
            let status = unsafe {
                let s = (*rapira_sg()).sapi_headers.http_response_code;
                if s != 0 { s as u16 } else { 200 }
            };

            if let Some(tx) = &c.tx {
                let _ = tx.send(Frame::Head(ResponseHead {
                    status,
                    headers: vec![],
                }));
                c.headers_sent = true;
            }
        }

        if let Some(tx) = &c.tx {
            let buf = unsafe { std::slice::from_raw_parts(buf as *const u8, len) };
            let _ = tx.send(Frame::Body(Bytes::copy_from_slice(buf)));
        }

        len
    })
}

fn split_header_line(line: &[u8]) -> Option<(String, String)> {
    let i = line.iter().position(|&b| b == b':')?;
    let k = String::from_utf8_lossy(&line[..i]).trim().to_string();
    let v = String::from_utf8_lossy(&line[i + 1..]).trim().to_string();
    (!k.is_empty()).then_some((k, v))
}

pub(crate) unsafe extern "C" fn send_headers(h: *mut sapi_headers_struct) -> c_int {
    guard(SAPI_HEADER_SEND_FAILED as c_int, || {
        let Some(c) = ctx() else {
            return SAPI_HEADER_SEND_FAILED as c_int;
        };
        let h = unsafe { &*h };
        let status = if h.http_response_code != 0 {
            h.http_response_code as u16
        } else {
            200
        };
        let mut headers = Vec::new();
        let mut el = h.headers.head;

        while !el.is_null() {
            let e = unsafe { &*el };
            let sh = unsafe { &*(e.data.as_ptr() as *const sapi_header_struct) };

            if !sh.header.is_null() && sh.header_len > 0 {
                let line =
                    unsafe { std::slice::from_raw_parts(sh.header as *const u8, sh.header_len) };

                if let Some(kv) = split_header_line(line) {
                    headers.push(kv);
                }
                el = e.next;
            }
        }

        if let Some(tx) = &c.tx {
            let _ = tx.send(Frame::Head(ResponseHead { status, headers }));
            c.headers_sent = true;
        }

        SAPI_HEADER_SENT_SUCCESSFULLY as c_int
    })
}

pub(crate) unsafe extern "C" fn flush(_sc: *mut c_void) {
    // nothing to do, chunks already sent
}

pub(crate) unsafe extern "C" fn read_post(buf: *mut c_char, count: usize) -> usize {
    guard(0, || {
        let Some(c) = ctx() else { return 0 };
        let dst = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, count) };
        c.req.body.read(dst).unwrap_or(0)
    })
}

pub(crate) unsafe extern "C" fn read_cookies() -> *mut c_char {
    guard(std::ptr::null_mut(), || match ctx() {
        Some(c) => {
            c.c.cookie.as_ptr() as *mut c_char // we own the buffer
        }
        None => std::ptr::null_mut(),
    })
}

pub(crate) unsafe extern "C" fn register_server_variables(track_vars_array: *mut zval) {
    guard((), || {
        let Some(c) = ctx() else { return };
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
        put("REQUEST_METHOD", &c.req.method);
        put("REQUEST_URI", &c.req.uri);
        put("QUERY_STRING", &c.req.query);
        put("SCRIPT_FILENAME", &c.req.script_filename.to_string_lossy());
        put("SCRIPT_NAME", &c.req.script_name);
        put("SERVER_PROTOCOL", &c.req.protocol);
        put("SERVER_SOFTWARE", "Rapira");
        put("SERVER_NAME", &c.req.server_name);
        put("SERVER_PORT", &c.req.server_port);
        put("REMOTE_ADDR", &c.req.remote_addr);

        if let Some(ct) = &c.req.content_type {
            put("CONTENT_TYPE", ct);
        }
        if c.req.content_length >= 0 {
            put("CONTENT_LENGTH", &c.req.content_length.to_string());
        }

        for (k, v) in &c.req.headers {
            put(
                &format!("HTTP_{}", k.to_ascii_uppercase().replace("-", "_")),
                v,
            );
        }
        for (k, v) in &c.req.server_vars {
            put(k, v);
        }
    })
}

pub(crate) unsafe extern "C" fn getenv_cb(_n: *const c_char, _l: usize) -> *mut c_char {
    std::ptr::null_mut()
}

pub(crate) unsafe extern "C" fn log_message(_message: *const c_char, _message_len: c_int) {
    // TODO: double check with php-src/other impl for the correct way to log messages from PHP
    let s = unsafe { std::ffi::CStr::from_ptr(_message) }
        .to_string_lossy()
        .to_owned();
    eprintln!("[php] {s}");
}
