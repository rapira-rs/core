use crate::context::{ctx, with_ctx};
use crate::types::{Context, Frame, ResponseHead};
use crate::*;
use bytes::Bytes;
use core::slice;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;

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
        let mut el: *mut _zend_llist_element = unsafe { &mut *self.0 }.headers.head;
        std::iter::from_fn(move || {
            let e: &_zend_llist_element = unsafe { el.as_ref()? };
            el = e.next;
            Some(SapiHeader(e.data.as_ptr() as *const sapi_header_struct))
        })
    }
}

struct SapiHeader(*const sapi_header_struct);

impl SapiHeader {
    fn name_value(&self) -> Option<(String, Vec<u8>)> {
        let sh = unsafe { &*self.0 };
        if sh.header.is_null() || sh.header_len == 0 {
            return None;
        }
        let line: &[u8] = unsafe { slice::from_raw_parts(sh.header as *const u8, sh.header_len) };
        let i: usize = line.iter().position(|&b| b == b':')?;
        let k: String = String::from_utf8_lossy(&line[..i]).trim().to_string();
        let v: Vec<u8> = line[i + 1..].trim_ascii().to_vec();
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
    SUCCESS
}
pub(crate) unsafe extern "C" fn sapi_deactivate_cb() -> c_int {
    SUCCESS
}

pub(crate) unsafe extern "C" fn ub_write(buf: *const c_char, len: usize) -> usize {
    guard(0, || {
        let ctx = unsafe {
            let Some(c) = ctx() else {
                let data = slice::from_raw_parts(buf as *const u8, len);
                log::info!(target: "php", "{}", String::from_utf8_lossy(data));
                return len;
            };
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
                ctx.finish();
                // receiver dropped = client disconnect; core never checks ub_write's
                // return, the SAPI must raise the abort itself (bails unless ignore_user_abort)
                unsafe {
                    // PHPAPI void php_handle_aborted_connection(void)
                    // {
                    //       PG(connection_status) = PHP_CONNECTION_ABORTED;
                    //       php_output_set_status(PHP_OUTPUT_DISABLED);

                    //       if (!PG(ignore_user_abort)) {
                    //               zend_bailout();
                    //       }
                    // }
                    php_handle_aborted_connection();
                    // function bails. q: what about longjum over rust frames? do we have tmp own of heap memory here?
                };
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
                // the problem here, is that if we don't send
                // SAPI_HEADER_SENT_SUCCESSFULLY, PHP will not cann ub_write
                // so send this status to allow in bootstrap to use the ub_write
                // php_output_header called first
                // then we have a gate:
                // if (context.out.data && context.out.used) {
                //     php_output_header(); <-- our handler (this)
                //     if (!(OG(flags) & PHP_OUTPUT_DISABLED)) { <-- if we sent failed
                //         sapi_module.ub_write(...); <-- boom, we don't call ub_write when ctx is null
                return SAPI_HEADER_SENT_SUCCESSFULLY as c_int;
            };

            if ctx.headers_sent {
                return SAPI_HEADER_SENT_SUCCESSFULLY as c_int;
            }

            ctx
        };

        let h = SapiHeaders(h);
        let headers: Vec<(String, Vec<u8>)> = h
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
    // php-src main/SAPI.c:232-252:
    // read_bytes = sapi_module.read_post(buffer, buflen);
    // ...
    // if (read_bytes < buflen) {
    // /* done */
    //     SG(post_read) = 1;
    // }
    with_ctx(0, |ctx| {
        let dst = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, count) };
        let mut filled = 0;
        while filled < count {
            match ctx.req.body.read(&mut dst[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return 0,
            }
        }
        filled
    })
}
pub(crate) unsafe extern "C" fn read_cookies() -> *mut c_char {
    with_ctx(null_mut(), |ctx| {
        ctx.c.cookie.as_ptr() as *mut c_char // we own the buffer
    })
}
pub(crate) unsafe extern "C" fn register_server_variables(track_vars_array: *mut zval) {
    with_ctx((), |ctx| {
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
        put("GATEWAY_INTERFACE", "CGI/1.1");
        put("HTTPS", if ctx.req.https { "on" } else { "" });

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
            let user: &CStr = unsafe { CStr::from_ptr(auth_user as *const c_char) };
            put("REMOTE_USER", &user.to_string_lossy());
        }

        if let Some(ct) = &ctx.req.content_type {
            put("CONTENT_TYPE", ct);
        }
        if ctx.req.content_length >= 0 {
            put("CONTENT_LENGTH", &ctx.req.content_length.to_string());
        }

        let mut merged: Vec<(String, String)> = Vec::with_capacity(ctx.req.headers.len());
        for (k, v) in &ctx.req.headers {
            let name = format!("HTTP_{}", k.to_ascii_uppercase().replace("-", "_"));
            match merged.iter_mut().find(|(n, _)| *n == name) {
                Some((n, val)) => {
                    val.push_str(if n == "HTTP_COOKIE" { "; " } else { ", " });
                    val.push_str(v);
                }
                None => {
                    merged.push((name, v.clone()));
                }
            }
        }
        for (k, v) in &merged {
            put(k, v);
        }

        for (k, v) in &ctx.req.server_vars {
            put(k, v);
        }
    })
}
pub(crate) unsafe extern "C" fn getenv_cb(name: *const c_char, name_len: usize) -> *mut c_char {
    with_ctx(null_mut(), |ctx| {
        if name.is_null() {
            return null_mut();
        }

        let key = unsafe { slice::from_raw_parts(name as *const u8, name_len) };
        ctx.c
            .env
            .get(key)
            .map_or(null_mut(), |v| v.as_ptr() as *mut c_char)
    })
}
fn syslog_to_level(syslog_lev: c_int) -> log::Level {
    match syslog_lev {
        0 => log::Level::Error, // LOG_EMERG
        1 => log::Level::Error, // LOG_ALERT
        2 => log::Level::Error, // LOG_CRIT
        3 => log::Level::Error, // LOG_ERR
        4 => log::Level::Warn,  // LOG_WARNING
        5 => log::Level::Info,  // LOG_NOTICE
        6 => log::Level::Info,  // LOG_INFO
        7 => log::Level::Debug, // LOG_DEBUG
        _ => log::Level::Info,
    }
}
pub(crate) unsafe extern "C" fn log_message(message: *const c_char, syslog_type: c_int) {
    guard((), || {
        if message.is_null() {
            return;
        }

        let s = unsafe { CStr::from_ptr(message).to_string_lossy() };
        log::log!(target: "php", syslog_to_level(syslog_type), "{s}");
    })
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
pub(crate) fn send_error_head(c: &mut Context, status: u16) {
    if c.headers_sent {
        return;
    }

    if let Some(tx) = &c.sender {
        let _ = tx.blocking_send(Frame::Head(ResponseHead {
            status,
            headers: vec![],
        }));
        c.headers_sent = true;
    }
}
