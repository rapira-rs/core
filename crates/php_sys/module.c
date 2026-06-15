#include "wrapper.h"

extern bool rapira_rs_handle_request(zval *handler);
extern void rapira_rs_finish_request(void);

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_rapira_handle_request, 0, 1, _IS_BOOL, 0)
    ZEND_ARG_INFO(0, handler)
ZEND_END_ARG_INFO()
ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_rapira_finish_request, 0, 0, _IS_BOOL, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(rapira_handle_request) {
    zval *handler;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_ZVAL(handler)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_BOOL(rapira_rs_handle_request(handler));
}
PHP_FUNCTION(rapira_finish_request) {
    ZEND_PARSE_PARAMETERS_NONE();
    rapira_rs_finish_request();
    RETURN_TRUE;
}

static const zend_function_entry rapira_functions[] = {
    PHP_FE(rapira_handle_request, arginfo_rapira_handle_request)
    PHP_FE(rapira_finish_request, arginfo_rapira_finish_request)
    PHP_FE_END
};

zend_module_entry rapira_module_entry = {
    STANDARD_MODULE_HEADER,
    "rapira", rapira_functions,
    NULL, NULL, NULL, NULL, NULL,        /* MINIT, MSHUTDOWN, RINIT, RSHUTDOWN, MINFO */
    "0.1.0",
    STANDARD_MODULE_PROPERTIES
};