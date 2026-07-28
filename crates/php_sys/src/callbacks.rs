use crate::context::{ctx, with_ctx};
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
        let Some(field) = split_header_line(line) else {
            log::debug!(
                target: "php",
                "dropped unrepresentable response header: {}",
                String::from_utf8_lossy(line)
            );
            return None;
        };
        Some(field)
    }
}

/// `tchar`, the only bytes a field name may contain (RFC 9110 §5.6.2,
/// https://www.rfc-editor.org/rfc/rfc9110#section-5.6.2; `field-name = token`, §5.1).
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// A byte a field value may carry: visible ASCII, obs-text (`0x80..=0xff`), SP or HTAB.
/// Excludes the C0 controls and DEL, which a `Vec<u8>` can hold but no field value may.
///
/// Wider than `field-vchar` itself (RFC 9110 §5.5 puts `VCHAR` at `%x21-7E`) because SP and
/// HTAB are legal *between* vchars in a `field-value`; the caller has already trimmed the
/// ends, so only interior whitespace reaches here.
fn is_field_value_byte(b: u8) -> bool {
    (b >= 0x20 && b != 0x7f) || b == b'\t'
}

/// Split one emitted header line into a name and value, rejecting anything a front
/// could not put on the wire. `sapi_header_op` screens only CR, LF and NUL, so a name
/// with a space or a value with a C0 control still reaches us; passing one on would
/// fail the whole response at the front instead of just that field.
fn split_header_line(line: &[u8]) -> Option<(String, Vec<u8>)> {
    let i: usize = line.iter().position(|&b| b == b':')?;
    let name: &[u8] = line[..i].trim_ascii();
    let value: &[u8] = line[i + 1..].trim_ascii();
    if name.is_empty() || !name.iter().all(|&b| is_tchar(b)) {
        return None;
    }
    if !value.iter().all(|&b| is_field_value_byte(b)) {
        return None;
    }
    // tchar is ASCII, so the name is UTF-8 by construction.
    Some((std::str::from_utf8(name).ok()?.to_owned(), value.to_vec()))
}

/// The CGI meta-variable name for a request field: `HTTP_` + the name uppercased with
/// `-` rewritten to `_` (RFC 3875 §4.1.18,
/// https://www.rfc-editor.org/rfc/rfc3875#section-4.1.18). Bytes in, bytes out; the
/// spare byte of capacity is the NUL `CString::new` appends.
///
/// `php_register_variable_ex` mangles once more on the way in — ` ` and `.` become `_`,
/// `[` opens array syntax — so this is not necessarily the final `$_SERVER` key.
fn cgi_header_name(field: &str) -> CString {
    let mut name: Vec<u8> = Vec::with_capacity(b"HTTP_".len() + field.len() + 1);
    name.extend_from_slice(b"HTTP_");
    for &b in field.as_bytes() {
        name.push(if b == b'-' {
            b'_'
        } else {
            b.to_ascii_uppercase()
        });
    }
    CString::new(name).unwrap_or_default()
}

/// The owned `$_SERVER` batch: `CONTENT_LENGTH`, one variable per request field, then
/// extra server vars. Kept separate from [`register_server_variables`] because it is the
/// half that allocates: it must run to completion before the first register call, so that
/// frame stays a POF (see the note there).
///
/// Returns `ManuallyDrop` rather than a plain `Vec` so the caller cannot hold this across a
/// register call without saying so: a live `Vec` + `CString`s there is drop glue on a frame a
/// `zend_bailout` longjmps straight past. Reclaim it with `ManuallyDrop::into_inner` on the
/// success path.
///
/// One entry per field name in, one out — repeats are combined before this point, so nothing
/// here has to look back at what it already pushed.
fn cgi_header_vars<'a>(
    headers: &'a [(String, Vec<u8>)],
    content_length: i64,
    server_vars: &'a [(String, String)],
) -> ManuallyDrop<Vec<(CString, Cow<'a, [u8]>)>> {
    let mut pairs: Vec<(CString, Cow<'a, [u8]>)> =
        Vec::with_capacity(1 + headers.len() + server_vars.len());

    if content_length >= 0 {
        pairs.push((
            c"CONTENT_LENGTH".to_owned(),
            Cow::Owned(content_length.to_string().into_bytes()),
        ));
    }
    for (field, value) in headers {
        pairs.push((cgi_header_name(field), Cow::Borrowed(value.as_slice())));
    }
    for (name, value) in server_vars {
        pairs.push((
            CString::new(name.as_str()).unwrap_or_default(),
            Cow::Borrowed(value.as_bytes()),
        ));
    }
    ManuallyDrop::new(pairs)
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
            put_bytes(c"CONTENT_TYPE", ct);
        }

        // Dynamic vars: owned names/values built up front by cgi_header_vars, then registered from
        // a ManuallyDrop so no drop is pending across the register loop (see the POF note above).
        // Registration order — CONTENT_LENGTH, HTTP_* headers, then extra server vars — is
        // load-bearing: php_register_variable_safe overwrites a same-named entry (last write wins),
        // so extra server vars registered last take precedence over the derived HTTP_* /
        // CONTENT_LENGTH vars.
        let pairs = cgi_header_vars(
            &ctx.req.headers,
            ctx.req.content_length,
            &ctx.req.server_vars,
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.as_bytes().to_vec()))
            .collect()
    }

    fn names(pairs: &[(CString, Cow<[u8]>)]) -> Vec<String> {
        pairs
            .iter()
            .map(|(n, _)| n.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn header_line_needs_a_colon_and_a_name() {
        assert!(split_header_line(b"no colon here").is_none());
        assert!(split_header_line(b": value").is_none());
        assert!(split_header_line(b"   : value").is_none());
    }

    #[test]
    fn header_line_trims_both_halves() {
        let (name, value) = split_header_line(b"X-Trace :  hello  ").unwrap();
        assert_eq!(name, "X-Trace");
        assert_eq!(value, b"hello");
    }

    /// sapi_header_op screens only CR, LF and NUL, so these still arrive here — and a
    /// front would reject the whole response rather than just the offending field.
    #[test]
    fn unrepresentable_fields_are_rejected() {
        assert!(split_header_line(b"Content Type: text/html").is_none());
        assert!(split_header_line(b"X-Trace: \x01").is_none());
        assert!(split_header_line(b"X-Trace: a\x7fb").is_none());
    }

    #[test]
    fn obs_text_and_underscores_stay_legal() {
        assert_eq!(
            split_header_line(b"X-Bin: \xff\xfe").unwrap().1,
            b"\xff\xfe"
        );
        // `_` is a tchar, so a response field may carry one.
        assert_eq!(split_header_line(b"X_Custom: 1").unwrap().0, "X_Custom");
    }

    /// The front screens field names against an allowlist of `[A-Za-z0-9-]`, which is only
    /// provably complete while this mapper rewrites nothing but `-`. Widening it here
    /// without revisiting that screen must fail loudly.
    #[test]
    fn cgi_header_name_rewrites_only_dash() {
        assert_eq!(cgi_header_name("x-foo").to_bytes(), b"HTTP_X_FOO");
        assert_eq!(cgi_header_name("x_foo").to_bytes(), b"HTTP_X_FOO");
        assert_eq!(cgi_header_name("x.foo").to_bytes(), b"HTTP_X.FOO");
        assert_eq!(cgi_header_name("x~foo").to_bytes(), b"HTTP_X~FOO");
    }

    /// php_register_variable_safe is last-write-wins, so batch order *is* the precedence
    /// rule: CONTENT_LENGTH, then HTTP_*, then host-supplied server vars.
    #[test]
    fn registration_order_gives_server_vars_precedence() {
        let headers = hdrs(&[("accept", "text/*")]);
        let server_vars = [("HTTP_ACCEPT".to_owned(), "override".to_owned())];
        // into_inner because cgi_header_vars hands back a ManuallyDrop: outside the callback
        // there is no bailout to survive, so the batch is reclaimed normally.
        let pairs = ManuallyDrop::into_inner(cgi_header_vars(&headers, 12, &server_vars));
        assert_eq!(
            names(&pairs),
            ["CONTENT_LENGTH", "HTTP_ACCEPT", "HTTP_ACCEPT"]
        );
        assert_eq!(pairs[0].1.as_ref(), b"12");
        assert_eq!(pairs[2].1.as_ref(), b"override");
    }
}
