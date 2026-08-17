use crate::{callbacks::guard, types::Context, *};
use std::{
    ffi::c_char,
    os::raw::c_void,
    ptr::{null, null_mut},
};

/// # Safety
/// The returned `&mut` aliases PHP's per-thread `SG(server_context)`. It is sound only because each
/// worker thread services exactly one request at a time (context is bound at request start, cleared
/// at finish). Callers must not hold the reference across another `ctx()` call on the same thread.
pub unsafe fn ctx<'a>() -> Option<&'a mut Context> {
    unsafe { ((*rapira_sg()).server_context as *mut Context).as_mut() }
}

/// Run `f` with the request's bound `Context`, guarded against unwind.
/// Returns `default` on panic or when no context is bound.
pub fn with_ctx<T: Copy>(default: T, f: impl FnOnce(&mut Context) -> T) -> T {
    guard(default, move || match unsafe { ctx() } {
        Some(ctx) => f(ctx),
        None => default,
    })
}

pub(crate) fn bind_server_context(ctx: &mut Context) {
    unsafe {
        (*rapira_sg()).server_context = (ctx as *mut Context) as *mut c_void;
    }
}

pub(crate) fn unbind_server_context() {
    unsafe {
        let sg = &mut *rapira_sg();
        sg.server_context = null_mut();
        // populate_request_context / read_cookies point these at the bound job.ctx's CStrings;
        // rapira_request_teardown (module.c) normally NULLs them, but a Rust panic can recycle
        // before teardown runs, dropping job.ctx while they still dangle. Clearing here on every
        // unbind (idempotent once teardown already cleared them) keeps a later
        // php_request_shutdown off freed memory.
        let ri = &mut sg.request_info;
        ri.request_method = null();
        ri.query_string = null_mut();
        ri.request_uri = null_mut();
        ri.path_translated = null_mut();
        ri.content_type = null();
        ri.cookie_data = null_mut();
    }
}

/// sapi_activate hard-resets proto_num to 1000 (main/SAPI.c:448), and its one
/// consumer is sapi_header_op's Location arm (302 vs 303 for non-GET/HEAD,
/// main/SAPI.c:836-842) - so the request protocol must be applied after
/// activation, not in [`populate_request_context`], which runs before it.
pub(crate) unsafe fn apply_proto_num(ctx: &Context) {
    if ctx.c.is_none() {
        return;
    }
    let sg = unsafe { &mut *rapira_sg() };
    sg.request_info.proto_num = match ctx.req.protocol.as_str() {
        "HTTP/1.0" => 1000,
        "HTTP/1.1" => 1001,
        p if p.starts_with("HTTP/2.0") => 2000,
        p if p.starts_with("HTTP/3.0") => 3000,
        _ => 1001,
    };
}

pub(crate) unsafe fn populate_request_context(ctx: &mut Context) {
    // CGI-only: the SAPI pointers below live in ReqC, which the dispatcher path
    // never builds (it binds no server context either).
    let Some(reqc) = ctx.c.as_ref() else { return };
    let sg = unsafe { &mut *rapira_sg() };
    // the engine never resets the previous request status (the reset in sapi_activate()
    // is commented out in php-src, main/SAPI.c:435-437)
    sg.sapi_headers.http_response_code = 200;
    let ri: &mut sapi_request_info = &mut sg.request_info;
    ri.request_method = reqc.method.as_ptr();
    ri.query_string = reqc.query.as_ptr() as *mut c_char;
    ri.request_uri = reqc.uri.as_ptr() as *mut c_char;
    ri.path_translated = reqc.script.as_ptr() as *mut c_char;
    ri.content_type = reqc.ctype.as_ref().map_or(null(), |s| s.as_ptr());
    ri.content_length = ctx.req.content_length;

    // auth → $_SERVER[PHP_AUTH_USER|PHP_AUTH_PW|PHP_AUTH_DIGEST].
    // php-src parses the header and estrndup's the values into SG(request_info),
    // so sapi_deactivate_module -> efree auth. NULL-safe (main.c guards `auth`).
    unsafe {
        php_handle_auth_data(
            reqc.authorization
                .as_ref()
                .map_or(null(), |auth| auth.as_ptr()),
        )
    };
}
