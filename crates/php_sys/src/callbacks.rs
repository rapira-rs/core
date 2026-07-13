use crate::context::{ctx, with_ctx};
use crate::types::{Context, ResponseHead, StreamState};
use crate::*;
use core::slice;
use std::borrow::Cow;
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

/// SAPI output callback, invoked through the C shim `rapira_ub_write` (module.c).
/// On a client disconnect it sets `*aborted = true`; the shim then raises
/// `php_handle_aborted_connection()` AFTER this frame returns, so the abort's
/// longjmp never crosses this `catch_unwind` frame.
///
/// # Safety
/// `buf` must point at `len` readable bytes and `aborted` at a writable `bool`,
/// both valid for the call (PHP guarantees this when firing `ub_write`). Must run
/// on a worker thread whose `Context` is bound in `SG(server_context)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ub_write(
    buf: *const c_char,
    len: usize,
    aborted: *mut bool,
) -> usize {
    guard(0, || {
        let ctx = unsafe {
            let Some(c) = ctx() else {
                let data = slice::from_raw_parts(buf as *const u8, len);
                log::info!(target: "php", "{}", String::from_utf8_lossy(data));
                return len;
            };
            c
        };

        if ctx.stream == StreamState::NotSent {
            let status = unsafe { SapiHeaders(&mut (*rapira_sg()).sapi_headers).status() };
            ctx.head = Some(ResponseHead {
                status,
                headers: vec![],
            });
            ctx.stream = StreamState::HeadSent;
        }

        if let Some(tx) = &ctx.sender {
            if tx.is_closed() {
                // receiver dropped = client disconnect; the sealed frame is
                // undeliverable, so stop buffering. Core ignores ub_write's return —
                // report it so the C shim raises the abort after this frame unwinds.
                ctx.finish(false);
                unsafe { *aborted = true };
                return 0;
            }
            let buf = unsafe { slice::from_raw_parts(buf as *const u8, len) };
            ctx.body.extend_from_slice(buf);
            // Body output began *during the handler* (the teardown flush sets
            // `tearing_down` first): a later error now truncates the body.
            if !ctx.tearing_down {
                ctx.stream = StreamState::BodyStreamed;
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

            if ctx.stream != StreamState::NotSent {
                return SAPI_HEADER_SENT_SUCCESSFULLY as c_int;
            }

            ctx
        };

        let h = SapiHeaders(h);
        let headers: Vec<(String, Vec<u8>)> = h
            .lines()
            .filter_map(|l: SapiHeader| l.name_value())
            .collect();
        ctx.head = Some(ResponseHead {
            status: h.status(),
            headers,
        });
        ctx.stream = StreamState::HeadSent;
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
        // we own the buffer; NULL = no Cookie header (the SAPI convention)
        ctx.c
            .cookie
            .as_ref()
            .map_or(null_mut(), |c| c.as_ptr() as *mut c_char)
    })
}
pub(crate) unsafe extern "C" fn register_server_variables(track_vars_array: *mut zval) {
    with_ctx((), |ctx| {
        // Names are `c"…"` literals (zero alloc); `php_register_variable_safe` takes the value as
        // ptr+len, so values pass through with no `CString` allocation either — as `&str` for the
        // ASCII vars, as raw `&[u8]` for (binary-safe) header values. Only dynamic names (HTTP_*
        // headers, extra server vars) still need a per-name `CString`.
        let put_bytes = |name: &CStr, val: &[u8]| unsafe {
            php_register_variable_safe(
                name.as_ptr(),
                val.as_ptr() as *const c_char,
                val.len(),
                track_vars_array,
            );
        };
        let put = |name: &CStr, val: &str| put_bytes(name, val.as_bytes());
        let put_dyn_bytes = |name: &str, val: &[u8]| {
            let n = CString::new(name).unwrap_or_default();
            put_bytes(&n, val);
        };
        let put_dyn = |name: &str, val: &str| put_dyn_bytes(name, val.as_bytes());
        // mapping trust policy: https://www.php.net/manual/en/reserved.variables.server.php
        put(c"PHP_SELF", &ctx.req.script_name);
        let doc_uri = ctx
            .req
            .uri
            .split_once('?')
            .map_or(ctx.req.uri.as_str(), |(p, _)| p);
        put(c"DOCUMENT_URI", doc_uri);
        put(c"DOCUMENT_ROOT", &ctx.req.document_root);
        put(
            c"REQUEST_SCHEME",
            if ctx.req.https { "https" } else { "http" },
        );
        put(c"REMOTE_HOST", &ctx.req.remote_addr);
        put(c"REMOTE_PORT", &ctx.req.remote_port);
        put(c"REMOTE_IDENT", ""); // RFC 1413: "The REMOTE_IDENT variable is not set by default"
        put(c"REQUEST_METHOD", &ctx.req.method);
        put(c"REQUEST_URI", &ctx.req.uri);
        put(c"QUERY_STRING", &ctx.req.query);
        put(
            c"SCRIPT_FILENAME",
            &ctx.req.script_filename.to_string_lossy(),
        );
        put(c"SCRIPT_NAME", &ctx.req.script_name);
        put(c"SERVER_PROTOCOL", &ctx.req.protocol);
        put(c"SERVER_SOFTWARE", "Rapira");
        put(c"SERVER_NAME", &ctx.req.server_name);
        put(c"SERVER_PORT", &ctx.req.server_port);
        put(c"REMOTE_ADDR", &ctx.req.remote_addr);
        put(c"GATEWAY_INTERFACE", "CGI/1.1");
        put(c"HTTPS", if ctx.req.https { "on" } else { "" });

        let auth_type = ctx
            .req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .and_then(|(_, v)| v.split(|b| b.is_ascii_whitespace()).find(|s| !s.is_empty()))
            .map(String::from_utf8_lossy)
            .unwrap_or_default();

        put(c"AUTH_TYPE", auth_type.as_ref());
        let auth_user = unsafe { (*rapira_sg()).request_info.auth_user };
        if !auth_user.is_null() {
            let user: &CStr = unsafe { CStr::from_ptr(auth_user as *const c_char) };
            put(c"REMOTE_USER", &user.to_string_lossy());
        }

        if let Some(ct) = &ctx.req.content_type {
            put(c"CONTENT_TYPE", ct);
        }
        if ctx.req.content_length >= 0 {
            put(c"CONTENT_LENGTH", &ctx.req.content_length.to_string());
        }

        let mut merged: Vec<(String, Cow<[u8]>)> = Vec::with_capacity(ctx.req.headers.len());
        for (k, v) in &ctx.req.headers {
            // `HTTP_<UPPERCASE, '-'→'_'>` built in a single allocation.
            let mut name = String::with_capacity(5 + k.len());
            name.push_str("HTTP_");
            name.extend(k.bytes().map(|b| {
                if b == b'-' {
                    '_'
                } else {
                    b.to_ascii_uppercase() as char
                }
            }));
            match merged.iter_mut().find(|(n, _)| *n == name) {
                Some((n, val)) => {
                    let sep: &[u8] = if n == "HTTP_COOKIE" { b"; " } else { b", " };
                    let owned = val.to_mut();
                    owned.extend_from_slice(sep);
                    owned.extend_from_slice(v);
                }
                // Borrow the header bytes; only an actual duplicate forces an owned copy.
                None => merged.push((name, Cow::Borrowed(v.as_slice()))),
            }
        }
        for (k, v) in &merged {
            put_dyn_bytes(k, &v[..]);
        }

        for (k, v) in &ctx.req.server_vars {
            put_dyn(k, v);
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
pub(crate) fn send_error_head(c: &mut Context, status: u16) {
    if c.stream != StreamState::NotSent {
        return;
    }
    c.head = Some(ResponseHead {
        status,
        headers: vec![],
    });
    c.stream = StreamState::HeadSent;
}
