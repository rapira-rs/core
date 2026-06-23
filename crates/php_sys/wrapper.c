#include "wrapper.h"

/* ZTS: via TSRM SAPI.h:156
 NTS: SAPI.h:160-161 */

sapi_globals_struct *rapira_sg(void) {
#ifdef ZTS
    return TSRMG_FAST_BULK(sapi_globals_offset, sapi_globals_struct *);
#else
    return &sapi_globals;
#endif
}

zend_executor_globals *rapira_eg(void) {
#ifdef ZTS
    return TSRMG_FAST_BULK(executor_globals_offset, zend_executor_globals *);
#else
    return &executor_globals;
#endif
}

php_core_globals *rapira_pg(void) {
#ifdef ZTS
    return TSRMG_FAST_BULK(core_globals_offset, php_core_globals *);
#else
    return &core_globals;
#endif
}

zend_compiler_globals *rapira_cg(void) {
#ifdef ZTS
    return TSRMG_FAST_BULK(compiler_globals_offset, zend_compiler_globals *);
#else
    return &compiler_globals;
#endif
}

void rapira_init_call_stack(void) {
#ifdef ZEND_CHECK_STACK_LIMIT
    zend_call_stack_init();
#endif
}