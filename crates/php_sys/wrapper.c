#include "wrapper.h"

/* NTS: direct globals, SAPI.h:160-161 */

sapi_globals_struct *rapira_sg(void) {
    return &sapi_globals;
}

php_core_globals *rapira_pg(void) {
    return &core_globals;
}

void rapira_init_call_stack(void) {
#ifdef ZEND_CHECK_STACK_LIMIT
    zend_call_stack_init();
#endif
}
