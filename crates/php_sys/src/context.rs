use crate::{types::Context, *};
use std::{ffi::c_char, os::raw::c_void};

pub(crate) fn bind_server_context(ctx: &mut Context) {
    unsafe {
        (*rapira_sg()).server_context = (ctx as *mut Context) as *mut c_void;
    }
}

pub(crate) fn unbind_server_context() {
    unsafe {
        (*rapira_sg()).server_context = std::ptr::null_mut();
    }
}

pub(crate) unsafe fn populate_request_context(ctx: &mut Context) {
    let ri = unsafe { &mut (*rapira_sg()).request_info };
    ri.request_method = ctx.c.method.as_ptr();
    ri.query_string = ctx.c.query.as_ptr() as *mut c_char;
    ri.request_uri = ctx.c.uri.as_ptr() as *mut c_char;
    ri.path_translated = ctx.c.script.as_ptr() as *mut c_char;
    ri.content_type = ctx
        .c
        .ctype
        .as_ref()
        .map_or(std::ptr::null(), |s| s.as_ptr());
    ri.content_length = ctx.req.content_length;
    ri.proto_num = match ctx.req.protocol.as_str() {
        "HTTP/1.0" => 1000,
        "HTTP/1.1" => 1001,
        "HTTP/2.0" => 2000,
        "HTTP/3.0" => 3000,
        _ => 0,
    };
}
