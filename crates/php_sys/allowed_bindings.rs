bind! {
    // types
    sapi_module_struct, sapi_headers_struct, sapi_header_struct, sapi_request_info,
    sapi_globals_struct, zend_executor_globals, php_core_globals, zend_compiler_globals,
    zend_llist, zend_llist_element, zend_file_handle, zend_module_entry, zend_function_entry,
    zend_execute_data, zend_auto_global, zend_string, zval, HashTable, zend_array, zend_long,
    zend_fcall_info, zend_fcall_info_cache,
    // functions
    sapi_startup, sapi_shutdown, sapi_activate, sapi_deactivate,
    php_module_startup, php_module_shutdown, php_request_startup, php_request_shutdown,
    php_execute_script, zend_error, zend_stream_init_filename, zend_destroy_file_handle,
    php_register_variable_safe, php_output_activate, php_output_deactivate, php_output_end_all,
    zend_activate_auto_globals, rapira_mode, RAPIRA_MODE_CLASSIC, RAPIRA_MODE_DISPATCHER,
    // the embedded-object layouts; wrapper.h is the source of truth
    rapira_exchange_obj, rapira_dispatcher_info_obj,
    // MINIT-written class-entry globals the Rust builder reads (static mut)
    rapira_ce_http_request, rapira_ce_http_multipart, rapira_ce_http_form_field,
    rapira_ce_http_uploaded_file, rapira_ce_http_tls, rapira_ce_inet_address,
    rapira_ce_unix_address, rapira_ce_already_finalized_error,
    rapira_ce_http_head_already_written_error, rapira_ce_internal_http_exchange,
    rapira_ce_internal_http_dispatcher, rapira_ce_internal_http_dispatcher_info,
    rapira_ce_timeout_exception, rapira_ce_closed_exception,
    rapira_ce_not_in_worker_mode_error, zend_argument_value_error, zend_argument_type_error,
    // writeHead's array walk: the exported position API (the FOREACH macros are
    // header-only); ZVAL_DEREF/Z_TYPE are field reads on the bindgen structs
    zend_hash_internal_pointer_reset_ex, zend_hash_get_current_key_ex,
    zend_hash_get_current_data_ex, zend_hash_move_forward_ex, HashPosition,
    zend_reference, IS_LONG, IS_NULL, IS_ARRAY, IS_REFERENCE,
    // throw surface: zend_throw_error/zend_value_error are varargs, called with
    // a fixed format; zend_throw_exception takes the message directly
    zend_throw_error, zend_value_error, zend_throw_exception,
    // the value-object constructor bodies (src/values.rs):
    // zend_update_property_str shares the ZPP-owned zend_string (addref, no
    // byte copy); instanceof_function is inline — its slow path is exported
    zend_update_property_str, instanceof_function_slow, zend_zval_value_name, IS_OBJECT,
    // Rapira\log()'s throwable flattener (src/dispatcher.rs): add_assoc_str /
    // add_index_zval are inline — add_assoc_stringl_ex and
    // zend_hash_index_update are their exported carriers; smart_str_free is
    // inline -> rapira_smart_str_free shim
    zend_read_property, zend_get_exception_base, zend_ce_throwable, php_json_encode,
    smart_str, PHP_JSON_PARTIAL_OUTPUT_ON_ERROR, add_assoc_stringl_ex,
    zend_hash_index_update, rapira_smart_str_free,
    // The Zend graph-builder surface (src/zend.rs + the exchange builder).
    // zend_update_property*: the EG(fake_scope) swap inside is what initializes
    // readonly props; never call write_property directly.
    object_init_ex, zend_update_property, zend_update_property_stringl,
    zend_update_property_long, zend_update_property_double, zend_update_property_null,
    // array_init_size is a macro -> rapira_array_init shim in wrapper.c
    rapira_array_init,
    // zend_symtable_str_update is inline; add_assoc_zval_ex is its exported
    // caller (zend_API.c), so symtable key normalization stays php-src's code
    add_assoc_zval_ex, add_next_index_stringl, add_next_index_object,
    zval_add_ref,
    zend_object, zend_class_entry, zend_result,
    zend_call_function, zend_fcall_info_init, zval_ptr_dtor, zend_hash_str_del,  // zval_ptr_dtor_nogc is inline-only -> use zval_ptr_dtor
    // zend_string_init is inline-only; the exported interner fn-pointer covers startup-time strings
    zend_hash_str_update, zend_string_init_interned,
    php_default_post_reader, php_default_treat_data, php_default_input_filter, zend_call_destructors,
    php_call_shutdown_functions, zend_observer_fcall_end_all, zend_unset_timeout, php_handle_auth_data, php_handle_aborted_connection,
    // consts
    SAPI_HEADER_SENT_SUCCESSFULLY, SAPI_HEADER_SEND_FAILED, SAPI_HEADER_DO_SEND,
    TRACK_VARS_FILES, NUM_TRACK_VARS, IS_UNDEF, IS_TRUE, IS_FALSE, IS_STRING, ZEND_OBSERVER_ENABLED,
    // diagnostic types; E_CORE and E_FATAL_ERRORS are php-src's own groupings
    E_WARNING, E_CORE_WARNING, E_COMPILE_WARNING, E_USER_WARNING,
    E_NOTICE, E_USER_NOTICE, E_DEPRECATED, E_USER_DEPRECATED,
    E_CORE, E_FATAL_ERRORS,
}
