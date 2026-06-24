use crate::{types::Context, *};
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

pub(crate) fn bind_server_context(ctx: &mut Context) {
    unsafe {
        (*rapira_sg()).server_context = (ctx as *mut Context) as *mut c_void;
    }
}

pub(crate) fn unbind_server_context() {
    unsafe {
        (*rapira_sg()).server_context = null_mut();
    }
}

pub(crate) unsafe fn populate_request_context(ctx: &mut Context) {
    let ri: &mut sapi_request_info = unsafe { &mut (*rapira_sg()).request_info };
    ri.request_method = ctx.c.method.as_ptr();
    ri.query_string = ctx.c.query.as_ptr() as *mut c_char;
    ri.request_uri = ctx.c.uri.as_ptr() as *mut c_char;
    ri.path_translated = ctx.c.script.as_ptr() as *mut c_char;
    ri.content_type = ctx.c.ctype.as_ref().map_or(null(), |s| s.as_ptr());
    ri.content_length = ctx.req.content_length;
    ri.proto_num = match ctx.req.protocol.as_str() {
        "HTTP/1.0" => 1000,
        "HTTP/1.1" => 1001,
        "HTTP/2.0" => 2000,
        "HTTP/3.0" => 3000,
        _ => 1001,
    };

    // auth → $_SERVER[PHP_AUTH_USER|PHP_AUTH_PW|PHP_AUTH_DIGEST].
    // php-src parses the header and estrndup's the values into SG(request_info),
    // so sapi_deactivate_module -> efree auth
    unsafe { php_handle_auth_data(ctx.c.authorization.as_ptr()) };
}
