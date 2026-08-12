#ifndef RAPIRA_CLASSES_H
#define RAPIRA_CLASSES_H

#include "wrapper.h"
#include "zend_API.h"
#include "zend_long.h"
#include "zend_property_hooks.h"

// rust glue
extern void rapira_rs_exchange_drop(void *job);

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
extern zend_class_entry *rapira_ce_http_tls;
extern zend_class_entry *rapira_ce_http_multipart;
extern zend_class_entry *rapira_ce_internal_http_dispatcher;
extern zend_class_entry *rapira_ce_inet_address;
extern zend_class_entry *rapira_ce_unix_address;

extern zend_class_entry *rapira_ce_internal_http_exchange;
extern zend_class_entry *rapira_ce_internal_http_dispatcher_info;
extern zend_class_entry *rapira_ce_http_head_already_written_error;
extern zend_class_entry *rapira_ce_http_head_not_written_error;
extern zend_class_entry *rapira_ce_http_content_length_exceeded_error;
extern zend_class_entry *rapira_ce_http_file_not_sendable_exception;
extern zend_class_entry *rapira_ce_http_form_field;
extern zend_class_entry *rapira_ce_http_uploaded_file;
extern zend_class_entry *rapira_ce_http_request;

// types in rapira.stub.php
// called from PHP_MINIT_FUNCTION
void rapira_register_classes(void);
// aka drop
void rapira_dispatcher_release(void);

// ext_functions[] - needs const initialization
const zend_function_entry *rapira_php_functions(void);

// https://www.zend.com/resources/php-extensions/embedding-c-data-into-php-objects
// +---------------------+  <- true allocation start
// | void *job           |     invisible to PHP
// | zval request        |
// +---------------------+  <- +RAPIRA_STD_OFFSET(rapira_exchange_obj)
// | zend_object std     |  <- THE pointer everyone else holds
// +---------------------+
typedef struct {
    void *job; // Box<ExchangeState> -> owned by Rust, NULLing when released
    zval request; // cached Rapira\Http\Request; IS_UNDEF until getRequest()
    zend_object std;
} rapira_exchange_obj;

typedef struct {
    zend_long pending;
    zend_long active;
    zend_object std;
} rapira_dispatcher_info_obj;

// https://github.com/php/php-src/pull/21899
// https://github.com/php/php-src/blob/7114314c5a96c362b95663f7e7c9184586721f58/UPGRADING.INTERNALS#L99-L100
// probably offsetof can be used on both, pre 8.6 and post 8.6
// but just to be safe, use XtOffsetOf on pre 8.6
#if PHP_VERSION_ID >= 80600
#define RAPIRA_STD_OFFSET(type) offsetof(type, std)
#else
#define RAPIRA_STD_OFFSET(type) XtOffsetOf(type, std)
#endif

// https://www.zend.com/resources/php-extensions/embedding-c-data-into-php-objects
// -> to inform the engine about special object layout
static zend_always_inline rapira_exchange_obj *
rapira_exchange_from(zend_object *obj) {
    // map the engine's zend_object pointer back to the enclosing struct: the
    // C fields sit before std, so step back by the offset (diagram above)
    return (rapira_exchange_obj *)((char *)obj -
                                   RAPIRA_STD_OFFSET(rapira_exchange_obj));
}

static zend_always_inline rapira_dispatcher_info_obj *
rapira_dispatcher_info_from(zend_object *obj) {
    // same as above, but for the dispatcher info object
    return (rapira_dispatcher_info_obj *)((char *)obj -
                                          RAPIRA_STD_OFFSET(
                                              rapira_dispatcher_info_obj));
}

#endif // RAPIRA_CLASSES_H
