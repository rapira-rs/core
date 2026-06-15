#ifndef RAPIRA_WRAPPER_H
#define RAPIRA_WRAPPER_H
#include <main/php.h>
#include <main/SAPI.h>
#include <main/php_main.h>
#include <main/php_variables.h>
#include <main/php_output.h>
#include <Zend/zend.h>
#include <Zend/zend_API.h>
#include <Zend/zend_globals.h>
#include <Zend/zend_compile.h>
#include <TSRM/TSRM.h>

sapi_globals_struct   *rapira_sg(void);
zend_executor_globals *rapira_eg(void);
php_core_globals      *rapira_pg(void);
zend_compiler_globals *rapira_cg(void);
#endif