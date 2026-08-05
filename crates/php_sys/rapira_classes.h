#ifndef RAPIRA_CLASSES_H
#define RAPIRA_CLASSES_H

#include "wrapper.h"
#include "zend_API.h"
#include "zend_property_hooks.h"

// rapira.stub.php
// log
extern zend_class_entry *rapira_ce_log_level;
extern zend_class_entry *rapira_ce_work;
extern zend_class_entry *rapira_ce_dispatcher_info;
extern zend_class_entry *rapira_ce_dispatcher;

// exceptions

extern zend_class_entry *rapira_ce_closed_exception;
extern zend_class_entry *rapira_ce_timeout_exception;
extern zend_class_entry *rapira_ce_work_discarded_exception;
extern zend_class_entry *rapira_ce_not_in_worker_mode_error;
extern zend_class_entry *rapira_ce_already_finalized_error;

// http
extern zend_class_entry *rapira_ce_http_inet_address;
extern zend_class_entry *rapira_ce_http_unix_address;
extern zend_class_entry *rapira_ce_http_tls;
extern zend_class_entry *rapira_ce_http_multipart;
extern zend_class_entry *rapira_ce_internal_http_dispatcher;

// types in rapira.stub.php
// called from PHP_MINIT_FUNCTION
void rapira_register_classes(void);
// aka drop
void rapira_dispatcher_release(void);

// ext_functions[] - needs const initialization
const zend_function_entry *rapira_php_functions(void);

#endif // RAPIRA_CLASSES_H
