#include "rapira_classes.h"
#include "rapira_arginfo.h"
#include "zend_API.h"

zend_class_entry *rapira_ce_log_level;
zend_class_entry *rapira_ce_work;
zend_class_entry *rapira_ce_dispatcher_info;
zend_class_entry *rapira_ce_dispatcher;

// return ext_functions from rapira_arginfo.h
const zend_function_entry *rapira_php_functions(void) { return ext_functions; }

void rapira_register_classes() {
    rapira_ce_log_level = register_class_Rapira_LogLevel();
    rapira_ce_work = register_class_Rapira_Work();
    rapira_ce_dispatcher_info = register_class_Rapira_DispatcherInfo();
    rapira_ce_dispatcher = register_class_Rapira_Dispatcher();
}
