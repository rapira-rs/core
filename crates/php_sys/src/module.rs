use crate::*;
use callbacks;
use std::{
    os::raw::c_char,
    ptr::{null, null_mut},
};

/// SAPI ini defaults, run by php_module_startup before any php.ini is parsed
/// (main/php_ini.c:420), so an explicit user value still wins. The built-in
/// defaults (main/main.c: display_errors=1, log_errors=0) fit an interactive
/// SAPI; a server wants every diagnostic delivered to the log_message callback
/// and none of them written into the response body.
unsafe extern "C" fn ini_defaults(configuration_hash: *mut HashTable) {
    for (name, value) in [(c"display_errors", c"0"), (c"log_errors", c"1")] {
        unsafe {
            // zend_startup installs the interner before php_init_config runs
            // (main/main.c:2249 vs 2288). Startup-stage interned strings are
            // permanent, so config_zval_dtor's zend_string_release is a no-op.
            let intern = zend_string_init_interned.expect("set by zend_startup");
            let mut v: zval = std::mem::zeroed();
            v.value.str_ = intern(value.as_ptr(), value.count_bytes(), true);
            v.u1.type_info = IS_STRING;
            zend_hash_str_update(
                configuration_hash,
                name.as_ptr(),
                name.count_bytes(),
                &mut v,
            );
        }
    }
}

pub(crate) fn build_sapi_module() -> sapi_module_struct {
    sapi_module_struct {
        // OPcache <= 8.4 starts only for SAPI names on accel_find_sapi()'s allowlist; an
        // unlisted name leaves accel_startup_ok false, so the pre-fork MINIT never creates
        // the SHM. "fastcgi" is on that allowlist and is referenced nowhere else in php-src.
        // 8.5 removed the allowlist:
        // https://github.com/php/php-src/commit/3088d6406847dd425dd43122f5de57cc97aa4408
        #[cfg(php84)]
        name: c"fastcgi".as_ptr() as *mut c_char,
        #[cfg(not(php84))]
        name: c"rapira".as_ptr() as *mut c_char,
        pretty_name: c"Rapira".as_ptr() as *mut c_char,
        startup: Some(callbacks::sapi_startup_cb),
        shutdown: Some(callbacks::sapi_shutdown_cb),
        activate: None,
        deactivate: Some(callbacks::sapi_deactivate_cb),
        ub_write: Some(rapira_ub_write),
        flush: Some(callbacks::flush),
        get_stat: None,
        getenv: Some(callbacks::getenv_cb),
        header_handler: None,
        send_headers: Some(callbacks::send_headers),
        send_header: None,
        read_post: Some(callbacks::read_post),
        read_cookies: Some(callbacks::read_cookies),
        register_server_variables: Some(callbacks::register_server_variables),
        log_message: Some(callbacks::log_message),
        get_request_time: None,
        terminate_process: None,
        php_ini_path_override: null_mut(),
        default_post_reader: Some(php_default_post_reader),
        treat_data: Some(php_default_treat_data),
        executable_location: null_mut(),
        php_ini_ignore: 0,
        php_ini_ignore_cwd: 0,
        get_fd: None,
        force_http_10: None,
        get_target_gid: None,
        get_target_uid: None,
        input_filter: Some(php_default_input_filter),
        ini_defaults: Some(ini_defaults),
        phpinfo_as_text: 0,
        ini_entries: null_mut(),
        additional_functions: null(),
        input_filter_init: None,
        sapi_error: Some(zend_error),
        #[cfg(php85)]
        pre_request_init: None,
    }
}
