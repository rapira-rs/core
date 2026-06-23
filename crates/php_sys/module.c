#include "wrapper.h"

extern bool rapira_rs_handle_request(zend_fcall_info *fci,
                                     zend_fcall_info_cache *fcc);
extern void rapira_rs_finish_request(void);

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_rapira_handle_request, 0, 1,
                                        _IS_BOOL, 0)
ZEND_ARG_INFO(0, handler)
ZEND_END_ARG_INFO()
ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_rapira_finish_request, 0, 0,
                                        _IS_BOOL, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(rapira_handle_request) {
    zend_fcall_info fci;
    zend_fcall_info_cache fcc;
    ZEND_PARSE_PARAMETERS_START(1, 1) // min-max
    Z_PARAM_FUNC(fci, fcc)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_BOOL(rapira_rs_handle_request(&fci, &fcc));
}

PHP_FUNCTION(rapira_finish_request) {
    ZEND_PARSE_PARAMETERS_NONE();
    rapira_rs_finish_request();
    RETURN_TRUE;
}

static const zend_function_entry rapira_functions[] = {
    PHP_FE(rapira_handle_request, arginfo_rapira_handle_request)
        PHP_FE(rapira_finish_request, arginfo_rapira_finish_request)
            PHP_FE_END};

zend_module_entry rapira_module_entry = {
    STANDARD_MODULE_HEADER,
    "rapira",
    rapira_functions,
    NULL,
    NULL,
    NULL,
    NULL,
    NULL, /* MINIT, MSHUTDOWN, RINIT, RSHUTDOWN, MINFO */
    "0.1.0",
    STANDARD_MODULE_PROPERTIES};

// runs per-request PHP work under a zend bailout guard
// we need to catch here zend_bailout(), but exit/die in php 8.4+ are not
// bailouts:
/*
// php-src Zend/zend_exceptions.c:1061
ZEND_API ZEND_COLD void zend_throw_unwind_exit(void) {
    ZEND_ASSERT(!EG(exception));
    EG(exception) = zend_create_unwind_exit();
    EG(opline_before_exception) = EG(current_execute_data)->opline;
    EG(current_execute_data)->opline = EG(exception_op);
}

exit calls zend_buildin_functions.c:142, and
1. zend_call_function returns normally with EG(exception) set
2. zend_throw_unwind_exit asserts that EG(exception)
*/

// in rust with repr[c]
enum {
    OK = 0,
    BAILOUT = 1,
    EXIT = 2,
    THROW = 3,
};

int rapira_run_handler(zend_fcall_info *fci, zend_fcall_info_cache *fcc) {
    int outcome = OK;
    zval retval;
    ZVAL_UNDEF(&retval);
    fci->size = sizeof *fci;
    fci->retval = &retval;
    fci->param_count = 0;
    fci->named_params = NULL;

    zend_try {
        zend_call_function(fci, fcc);
        zval_ptr_dtor(&retval);
    }
    zend_catch { outcome = BAILOUT; }
    zend_end_try();

    // exit()/die() and uncaught exceptions land in EG(exception), not bailouts
    if (EG(exception)) {
        if (zend_is_unwind_exit(EG(exception)) ||
            zend_is_graceful_exit(EG(exception))) {
            outcome = EXIT;
            zend_clear_exception();
        } else {
            outcome = THROW;
            zend_try { zend_exception_error(EG(exception), E_ERROR); }
            zend_end_try();
        }
    }

    zend_try { php_call_shutdown_functions(); }
    zend_catch {}
    zend_end_try();

    zend_try { zend_call_destructors(); }
    zend_catch {}
    zend_end_try();

    if (EG(exception)) {
        zend_clear_exception();
    }

    php_free_shutdown_functions();
    return outcome;
}