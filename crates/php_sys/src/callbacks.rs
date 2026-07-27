use crate::context::{ctx, with_ctx};
use crate::diagnostics::syslog_to_level;
use crate::types::{Context, StreamState};
use crate::*;
use core::slice;
use std::borrow::Cow;
use std::ffi::{CStr, CString};
use std::mem::ManuallyDrop;
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;

pub fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(default)
}

/// Hard cap on the buffered response body. The single-Frame contract buffers every
/// `ub_write` in Rust, outside PHP's `memory_limit`; a runaway response aborts (and
/// recycles the worker) instead of exhausting the host.
const MAX_BUFFERED_BODY: usize = 1 << 30; // 1 GiB

struct SapiHeaders(*mut sapi_headers_struct);

impl SapiHeaders {
    fn status(&self) -> u16 {
        let h = unsafe { &*self.0 };
        if h.http_response_code != 0 {
            // http_response_code is an app-controlled c_int; clamp to a valid HTTP status so the
            // u16 cast can't wrap (e.g. 70000 -> 4464).
            h.http_response_code.clamp(100, 599) as u16
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
                let data = slice::from_raw_parts(buf.cast::<u8>(), len);
                log::info!(target: "php", "{}", String::from_utf8_lossy(data));
                return len;
            };
            c
        };

        if ctx.stream == StreamState::NotSent {
            let status = unsafe { SapiHeaders(&mut (*rapira_sg()).sapi_headers).status() };
            ctx.commit_head(status, vec![]);
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
            if ctx.body.len() + len > MAX_BUFFERED_BODY {
                // over the buffer cap: seal the buffered body as truncated and abort the
                // request through the same path as a client disconnect
                ctx.finish(true);
                unsafe { *aborted = true };
                return 0;
            }
            let buf = unsafe { slice::from_raw_parts(buf.cast::<u8>(), len) };
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
                // php_output_op() calls sapi_module.ub_write() only when this hook
                // returns SAPI_HEADER_SENT_SUCCESSFULLY and OG(flags) lacks
                // PHP_OUTPUT_DISABLED (php-src main/output.c:1131-1138). Bootstrap
                // runs with no bound Context, so return success here to keep its
                // output reaching ub_write.
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
        ctx.commit_head(h.status(), headers);
        SAPI_HEADER_SENT_SUCCESSFULLY as c_int
    })
}

pub(crate) unsafe extern "C" fn flush(_sc: *mut c_void) {
    // nothing to do: the body buffers into the single Frame, delivered at finish
}
pub(crate) unsafe extern "C" fn read_post(buf: *mut c_char, count: usize) -> usize {
    // A short read (fewer than `count` bytes) signals end-of-body: the engine
    // sets SG(post_read)=1 and stops calling this (php-src main/SAPI.c:232-252).
    with_ctx(0, |ctx| {
        let dst = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), count) };
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
        // the Cookie buffer is request-owned; NULL = no Cookie header (the SAPI convention)
        ctx.c
            .cookie
            .as_ref()
            .map_or(null_mut(), |c| c.as_ptr() as *mut c_char)
    })
}
pub(crate) unsafe extern "C" fn register_server_variables(track_vars_array: *mut zval) {
    with_ctx((), |ctx| {
        // php_register_variable_safe emalloc's; under memory_limit/OOM it zend_error_noreturn ->
        // zend_bailout -> longjmp, which unwinds this frame back to rapira_request_activate's
        // zend_catch. A longjmp over a Rust frame with pending drops is UB, so across every register
        // call this frame must be a POF (no live owned Rust values). The fixed vars below register
        // from `c"…"` static names + up-stack `ctx.req`/`ctx.c` borrows only — already POF. Everything that
        // needs owned storage (CONTENT_LENGTH, HTTP_* header names/values, extra server vars) is
        // materialized into `pairs` first (pure Rust, cannot bail) and registered from a
        // ManuallyDrop (no drop glue), so a bail leaks that one batch instead of corrupting the
        // unwind. Same reason ub_write routes its abort longjmp through the C shim.
        let put_bytes = |name: &CStr, val: &[u8]| unsafe {
            php_register_variable_safe(
                name.as_ptr(),
                val.as_ptr() as *const c_char,
                val.len(),
                track_vars_array,
            );
        };
        let put = |name: &CStr, val: &str| put_bytes(name, val.as_bytes());
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
        // REMOTE_IDENT is optional per CGI/1.1; rapira runs no RFC 1413 ident lookup, so it is empty.
        // https://www.rfc-editor.org/rfc/rfc3875#section-4.1.10
        // https://www.rfc-editor.org/rfc/rfc1413
        put(c"REMOTE_IDENT", "");
        put(c"REQUEST_METHOD", &ctx.req.method);
        put(c"REQUEST_URI", &ctx.req.uri);
        put(c"QUERY_STRING", &ctx.req.query);
        // ctx.c.script is the same lossy-converted path as a request-lived CString; borrowing
        // it keeps this frame POF (to_string_lossy() on a non-UTF-8 path would allocate).
        put_bytes(c"SCRIPT_FILENAME", ctx.c.script.to_bytes());
        put(c"SCRIPT_NAME", &ctx.req.script_name);
        put(c"SERVER_PROTOCOL", &ctx.req.protocol);
        put(c"SERVER_SOFTWARE", "Rapira");
        put(c"SERVER_NAME", &ctx.req.server_name);
        put(c"SERVER_PORT", &ctx.req.server_port);
        put(c"REMOTE_ADDR", &ctx.req.remote_addr);
        put(c"GATEWAY_INTERFACE", "CGI/1.1");
        put(c"HTTPS", if ctx.req.https { "on" } else { "" });

        // raw token borrow (no String::from_utf8_lossy) keeps AUTH_TYPE POF; binary-safe anyway
        let auth_type: &[u8] = ctx
            .req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .and_then(|(_, v)| v.split(|b| b.is_ascii_whitespace()).find(|s| !s.is_empty()))
            .unwrap_or(b"");
        put_bytes(c"AUTH_TYPE", auth_type);

        let auth_user = unsafe { (*rapira_sg()).request_info.auth_user };
        if !auth_user.is_null() {
            // borrows the SAPI-owned buffer; to_bytes() (no owned copy) stays POF
            let user: &CStr = unsafe { CStr::from_ptr(auth_user as *const c_char) };
            put_bytes(c"REMOTE_USER", user.to_bytes());
        }

        if let Some(ct) = &ctx.req.content_type {
            put(c"CONTENT_TYPE", ct);
        }

        // Dynamic vars: owned names/values built up front, then registered from a ManuallyDrop so
        // no drop is pending across the register loop (see the POF note above). Registration order —
        // CONTENT_LENGTH, deduped HTTP_* headers, then extra server vars — is load-bearing:
        // php_register_variable_safe overwrites a same-named entry (last write wins), so extra
        // server vars registered last take precedence over the derived HTTP_* / CONTENT_LENGTH vars.
        let mut pairs: Vec<(CString, Cow<[u8]>)> =
            Vec::with_capacity(1 + ctx.req.headers.len() + ctx.req.server_vars.len());
        if ctx.req.content_length >= 0 {
            pairs.push((
                c"CONTENT_LENGTH".to_owned(),
                Cow::Owned(ctx.req.content_length.to_string().into_bytes()),
            ));
        }
        for (k, v) in &ctx.req.headers {
            // `HTTP_<UPPERCASE, '-'→'_'>` in one allocation; +1 spare byte so the
            // CString::new below appends its NUL without reallocating.
            let mut name = String::with_capacity(6 + k.len());
            name.push_str("HTTP_");
            name.extend(k.bytes().map(|b| {
                if b == b'-' {
                    '_'
                } else {
                    b.to_ascii_uppercase() as char
                }
            }));
            match pairs
                .iter_mut()
                .find(|(n, _)| n.as_bytes() == name.as_bytes())
            {
                Some((n, val)) => {
                    let sep: &[u8] = if n.as_bytes() == b"HTTP_COOKIE" {
                        b"; "
                    } else {
                        b", "
                    };
                    let owned = val.to_mut();
                    owned.extend_from_slice(sep);
                    owned.extend_from_slice(v);
                }
                // Borrow the header bytes; only an actual duplicate forces an owned copy.
                None => pairs.push((
                    CString::new(name).unwrap_or_default(),
                    Cow::Borrowed(v.as_slice()),
                )),
            }
        }
        for (k, v) in &ctx.req.server_vars {
            pairs.push((
                CString::new(k.as_str()).unwrap_or_default(),
                Cow::Borrowed(v.as_bytes()),
            ));
        }

        let pairs = ManuallyDrop::new(pairs);
        for (name, val) in pairs.iter() {
            put_bytes(name, &val[..]);
        }
        // success path: reclaim the batch (a bail above longjmps past this, leaking it)
        drop(ManuallyDrop::into_inner(pairs));
    })
}
pub(crate) unsafe extern "C" fn getenv_cb(name: *const c_char, name_len: usize) -> *mut c_char {
    with_ctx(null_mut(), |ctx| {
        if name.is_null() {
            return null_mut();
        }

        let key = unsafe { slice::from_raw_parts(name.cast::<u8>(), name_len) };
        ctx.c
            .env
            .get(key)
            .map_or(null_mut(), |v| v.as_ptr() as *mut c_char)
    })
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
    c.commit_head(status, vec![]);
}

/// Finish the response bookkeeping shared by both workers: compute the truncation flag and, when the
/// request errored without any head reaching the consumer, synthesize a head-only 500. A committed
/// head (a real one or a buffered body flushed at teardown) is already the complete response.
pub(crate) fn finalize_response(c: &mut Context, errored: bool) -> bool {
    let truncated = c.is_truncated(errored);
    if errored {
        send_error_head(c, 500); // no-op unless nothing committed a head yet
    }
    truncated
}
