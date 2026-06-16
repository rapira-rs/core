use crate::*;
use callbacks;
use std::os::raw::c_char;

pub(crate) fn build_sapi_module() -> sapi_module_struct {
    let mut m: sapi_module_struct = unsafe { std::mem::zeroed() };
    m.name = c"rapira".as_ptr() as *mut c_char;
    m.pretty_name = c"Rapira".as_ptr() as *mut c_char;
    m.startup = Some(callbacks::sapi_startup_cb);
    m.shutdown = Some(callbacks::sapi_shutdown_cb);
    m.deactivate = Some(callbacks::sapi_deactivate_cb);
    m.ub_write = Some(callbacks::ub_write);
    m.flush = Some(callbacks::flush);
    m.send_headers = Some(callbacks::send_headers);
    m.read_post = Some(callbacks::read_post);
    m.read_cookies = Some(callbacks::read_cookies);
    m.register_server_variables = Some(callbacks::register_server_variables);
    m.getenv = Some(callbacks::getenv_cb);
    m.log_message = Some(callbacks::log_message);
    m.sapi_error = Some(zend_error);
    m.default_post_reader = Some(php_default_post_reader);
    m.treat_data = Some(php_default_treat_data);
    m.input_filter = Some(php_default_input_filter);
    m
}
