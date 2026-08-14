#ifndef RAPIRA_CLASSES_H
#define RAPIRA_CLASSES_H

#include "wrapper.h"
#include "zend_API.h"
#include "zend_long.h"
#include "zend_property_hooks.h"

// rust glue
extern void rapira_rs_exchange_drop(void *job);
extern void rapira_rs_dispatcher_release(void);

// The class-entry externs and the object layouts live in wrapper.h (the
// bindgen input), included above.

// called from PHP_MINIT_FUNCTION
void rapira_register_classes(void);

// ext_functions[] - needs const initialization
const zend_function_entry *rapira_php_functions(void);

// XtOffsetOf pre-8.6; offsetof from 8.6.
// https://github.com/php/php-src/pull/21899
// https://github.com/php/php-src/blob/7114314c5a96c362b95663f7e7c9184586721f58/UPGRADING.INTERNALS#L99-L100
#if PHP_VERSION_ID >= 80600
#define RAPIRA_STD_OFFSET(type) offsetof(type, std)
#else
#define RAPIRA_STD_OFFSET(type) XtOffsetOf(type, std)
#endif

// a false return from a Rust half with a clean EG is a caught panic; backstop it
static zend_always_inline void rapira_throw_or_backstop(const char *what) {
    if (!EG(exception)) {
        zend_throw_error(NULL, "%s failed", what);
    }
}

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
