#include "wrapper.h"

sapi_globals_struct *rapira_sg(void) {
    return TSRMG_FAST_BULK(sapi_globals_offset, sapi_globals_struct *);
}
zend_executor_globals *rapira_eg(void) {
    return TSRMG_FAST_BULK(executor_globals_offset, zend_executor_globals *);
}
php_core_globals *rapira_pg(void) {
    return TSRMG_FAST_BULK(core_globals_offset, php_core_globals *);
}
zend_compiler_globals *rapira_cg(void) {
    return TSRMG_FAST_BULK(compiler_globals_offset, zend_compiler_globals *);
}