use crate::*;
use callbacks;
use std::os::raw::{c_char, c_int};

pub(crate) fn build_sapi_module() -> sapi_module_struct {
    let mut m: sapi_module_struct = unsafe { std::mem::zeroed() };
    m.name = c"rapira".as_ptr() as *mut c_char;
    m.pretty_name = c"Rapira".as_ptr() as *mut c_char;
    m.startup = Some(callbacks::sapi_startup_cb);
    m.shutdown = Some(callbacks::sapi_shutdown_cb);
    m.deactivate = Some(callbacks::sapi_deactivate_cb);
    m.ub_write = Some(callbacks::ub_write);
    // TODO: the rest of the callbacks
    m
}

