#ifndef RAPIRA_WRAPPER_H
#define RAPIRA_WRAPPER_H
// clang-format off
#include <TSRM/TSRM.h>
#include <Zend/zend.h>
#include <Zend/zend_API.h>
#include <Zend/zend_compile.h>
#include <Zend/zend_globals.h>
#include <Zend/zend_exceptions.h>
#include <main/php.h>
#include <ext/standard/basic_functions.h>
#include <main/SAPI.h>
#include <main/php_main.h>
#include <main/php_output.h>
#include <main/php_variables.h>
// clang-format on
#ifdef HAVE_PHP_SESSION
#include <ext/session/php_session.h>
#endif

sapi_globals_struct *rapira_sg(void);
zend_executor_globals *rapira_eg(void);
php_core_globals *rapira_pg(void);
zend_compiler_globals *rapira_cg(void);
void rapira_init_call_stack(void);
#endif