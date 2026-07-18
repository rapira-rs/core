#include "wrapper.h"

#if defined(PHP_WIN32) && defined(ZTS)
ZEND_TSRMLS_CACHE_DEFINE()
#endif

/* ZTS: via TSRM SAPI.h:156
 NTS: SAPI.h:160-161 */

sapi_globals_struct *rapira_sg(void) {
#ifdef ZTS
    return TSRMG_FAST_BULK(sapi_globals_offset, sapi_globals_struct *);
#else
    return &sapi_globals;
#endif
}

php_core_globals *rapira_pg(void) {
#ifdef ZTS
    return TSRMG_FAST_BULK(core_globals_offset, php_core_globals *);
#else
    return &core_globals;
#endif
}

void rapira_init_call_stack(void) {
#ifdef ZEND_CHECK_STACK_LIMIT
    zend_call_stack_init();
#endif
}

// Windows+ZTS keeps the TSRMLS cache as a per-module thread-local pointer; each thread must prime
// it before any SG()/EG()/PG()/CG() access or TSRMG_FAST_BULK reads a null cache. No-op elsewhere
// (Linux ZTS shares libphp's __thread cache; NTS has none).
void rapira_tsrmls_cache_update(void) {
#if defined(PHP_WIN32) && defined(ZTS)
    ZEND_TSRMLS_CACHE_UPDATE();
#endif
}