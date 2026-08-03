#include "rapira_classes.h"
#include "ext/spl/spl_exceptions.h"
#include "rapira_arginfo.h"
#include "rapira_exception_arginfo.h"
#include "zend_API.h"
#include "zend_exceptions.h"
#include "zend_property_hooks.h"

zend_class_entry *rapira_ce_log_level;
zend_class_entry *rapira_ce_work;
zend_class_entry *rapira_ce_dispatcher_info;
zend_class_entry *rapira_ce_dispatcher;

zend_class_entry *rapira_ce_closed_exception;
zend_class_entry *rapira_ce_timeout_exception;
zend_class_entry *rapira_ce_work_discarded_exception;
zend_class_entry *rapira_ce_not_in_worker_mode_error;
zend_class_entry *rapira_ce_already_finalized_error;

// return ext_functions from rapira_arginfo.h
const zend_function_entry *rapira_php_functions(void) { return ext_functions; }

void rapira_register_classes(void) {
    zend_class_entry *throwable =
        register_class_Rapira_Exception_RapiraThrowable(zend_ce_throwable);

    rapira_ce_closed_exception =
        register_class_Rapira_Exception_ClosedException(spl_ce_RuntimeException,
                                                        throwable);
    rapira_ce_timeout_exception =
        register_class_Rapira_Exception_TimeoutException(
            spl_ce_RuntimeException, throwable);
    rapira_ce_work_discarded_exception =
        register_class_Rapira_Exception_WorkDiscardedException(
            spl_ce_RuntimeException, throwable);
    rapira_ce_not_in_worker_mode_error =
        register_class_Rapira_Exception_NotInWorkerModeError(zend_ce_error,
                                                             throwable);
    rapira_ce_already_finalized_error =
        register_class_Rapira_Exception_AlreadyFinalizedError(zend_ce_error,
                                                              throwable);

    rapira_ce_log_level = register_class_Rapira_LogLevel();
    rapira_ce_work = register_class_Rapira_Work();
    rapira_ce_dispatcher_info = register_class_Rapira_DispatcherInfo();
    rapira_ce_dispatcher = register_class_Rapira_Dispatcher();
}
