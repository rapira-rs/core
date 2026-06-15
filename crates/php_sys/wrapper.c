#include "wrapper.h"
#ifdef ZTS
static void *rapira_tsrmg(int rsrc_id) {                 /* mirrors ext-php-rs ext_php_rs_tsrmg_bulk */
    return (*((void ***) tsrm_get_ls_cache()))[TSRM_UNSHUFFLE_RSRC_ID(rsrc_id)];
}
sapi_globals_struct   *rapira_sg(void){ return rapira_tsrmg(sapi_globals_id); }
zend_executor_globals *rapira_eg(void){ return rapira_tsrmg(executor_globals_id); }
php_core_globals      *rapira_pg(void){ return rapira_tsrmg(core_globals_id); }
zend_compiler_globals *rapira_cg(void){ return rapira_tsrmg(compiler_globals_id); }
#else
sapi_globals_struct   *rapira_sg(void){ return &sapi_globals; }
zend_executor_globals *rapira_eg(void){ return &executor_globals; }
php_core_globals      *rapira_pg(void){ return &core_globals; }
zend_compiler_globals *rapira_cg(void){ return &compiler_globals; }
#endif