#include "rapira_classes.h"
#include "ext/spl/spl_exceptions.h"
#include "rapira_arginfo.h"
#include "rapira_exception_arginfo.h"
#include "rapira_http_arginfo.h"
#include "zend_API.h"
#include "zend_exceptions.h"
#include "zend_object_handlers.h"
#include "zend_property_hooks.h"
#include "zend_types.h"

// logger
zend_class_entry *rapira_ce_log_level;
zend_class_entry *rapira_ce_work;
zend_class_entry *rapira_ce_dispatcher_info;
zend_class_entry *rapira_ce_dispatcher;

// exceptions
zend_class_entry *rapira_ce_closed_exception;
zend_class_entry *rapira_ce_timeout_exception;
zend_class_entry *rapira_ce_work_discarded_exception;
zend_class_entry *rapira_ce_not_in_worker_mode_error;
zend_class_entry *rapira_ce_already_finalized_error;

// http
zend_class_entry *rapira_ce_http_inet_address;
zend_class_entry *rapira_ce_http_unix_address;
zend_class_entry *rapira_ce_http_tls;
zend_class_entry *rapira_ce_http_multipart;
zend_class_entry *rapira_ce_internal_http_dispatcher;


// return ext_functions from rapira_arginfo.h
const zend_function_entry *rapira_php_functions(void) { return ext_functions; }
// we need to protect a copy
static zend_object_handlers rapira_host_handlers;

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

    // http stuff
    rapira_ce_http_inet_address = register_class_Rapira_Http_InetAddress();
    rapira_ce_http_unix_address = register_class_Rapira_Http_UnixAddress();
    rapira_ce_http_tls = register_class_Rapira_Http_Tls();
    register_class_Rapira_Http_FormField();
    register_class_Rapira_Http_UploadedFile();
    rapira_ce_http_multipart = register_class_Rapira_Http_Multipart();
    register_class_Rapira_Http_Request();

    zend_class_entry *http_info = register_class_Rapira_Http_HttpDispatcherInfo(
        rapira_ce_dispatcher_info);
    zend_class_entry *http_exchange =
        register_class_Rapira_Http_Exchange(rapira_ce_work);
    zend_class_entry *http_dispatcher =
        register_class_Rapira_Http_HttpDispatcher(rapira_ce_dispatcher);

    // exceptions
    register_class_Rapira_Http_Exception_ContentLengthExceededError(
        zend_ce_error, throwable);
    register_class_Rapira_Http_Exception_HeadAlreadyWrittenError(zend_ce_error,
                                                                 throwable);
    register_class_Rapira_Http_Exception_HeadNotWrittenError(zend_ce_error,
                                                             throwable);
    register_class_Rapira_Http_Exception_FileNotSendableException(
        spl_ce_RuntimeException, throwable);

    rapira_ce_internal_http_dispatcher =
        register_class_Rapira_Internal_Http_Dispatcher(http_dispatcher);
    zend_class_entry *internal_info =
        register_class_Rapira_Internal_Http_DispatcherInfo(http_info);
    zend_class_entry *internal_exchange =
        register_class_Rapira_Internal_Http_Exchange(http_exchange);

    // preventing cloning of internal objects
    // clone_call = zobj->handlers->clone_obj;
    // if (UNEXPECTED(clone_call == NULL)) {
    //     zend_throw_error(NULL, "Trying to clone an uncloneable object of class %s", ...);
    // adaptation of the zend_compile.c:2070
    // /Zend/zend_vm_def.h:6050-6056,
    memcpy(&rapira_host_handlers, &std_object_handlers,
           sizeof(rapira_host_handlers));
    rapira_host_handlers.clone_obj = NULL;
    rapira_ce_internal_http_dispatcher->default_object_handlers =
        &rapira_host_handlers;
    internal_info->default_object_handlers = &rapira_host_handlers;
    internal_exchange->default_object_handlers = &rapira_host_handlers;
}
