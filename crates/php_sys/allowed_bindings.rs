bind! {
    // types
    sapi_module_struct, sapi_headers_struct, sapi_header_struct, sapi_request_info,
    sapi_globals_struct, zend_executor_globals, php_core_globals, zend_compiler_globals,
    zend_llist, zend_llist_element, zend_file_handle, zend_module_entry, zend_function_entry,
    zend_execute_data, zend_auto_global, zend_string, zval, HashTable, zend_array, zend_long,
    zend_fcall_info, zend_fcall_info_cache, ZEND_RESULT_CODE,
    // functions
    sapi_startup, sapi_shutdown, sapi_activate, sapi_deactivate,
    php_module_startup, php_module_shutdown, php_request_startup, php_request_shutdown,
    php_execute_script, zend_error, zend_stream_init_filename, zend_destroy_file_handle,
    php_register_variable_safe, php_output_activate, php_output_deactivate, php_output_end_all,
    zend_activate_auto_globals, php_tsrm_startup_ex, tsrm_shutdown, ts_resource_ex, ts_free_thread,  // ts_resource is a macro -> ts_resource_ex
    zend_call_function, zend_fcall_info_init, zval_ptr_dtor, zend_hash_str_del,  // zval_ptr_dtor_nogc is inline-only -> use zval_ptr_dtor
    php_default_post_reader, php_default_treat_data, php_default_input_filter, zend_call_destructors,
    php_call_shutdown_functions, zend_observer_fcall_end_all, zend_unset_timeout,
    // consts
    SAPI_HEADER_SENT_SUCCESSFULLY, SAPI_HEADER_SEND_FAILED, SAPI_HEADER_DO_SEND,
    TRACK_VARS_FILES, NUM_TRACK_VARS, IS_UNDEF, IS_TRUE, IS_FALSE, ZEND_OBSERVER_ENABLED,
}
