#include "handle_config.h"
#include "rapira_arginfo.h"
#include "Zend/zend_smart_str.h"
#include "ext/json/php_json.h"

// Rust glue (crates/php_sys/src/handle_config.rs).
extern bool rapira_rs_worker_mode(void);
extern bool rapira_rs_runtime_info(rapira_runtime_info *out);
// Hands the plugin its PHP-declared config as an opaque JSON blob (copied).
extern void rapira_rs_set_handler_config(const uint8_t *ptr, size_t len);
// Rust: one worker-loop turn (crates/php_sys/src/rapira_worker.rs).
extern int rapira_rs_handle_request(zend_fcall_info *fci,
                                    zend_fcall_info_cache *fcc);

// What a worker-loop turn tells its caller to do next.
// Keep in sync with rapira_worker.rs HandleAction.
enum {
    RAPIRA_STOP = 0,
    RAPIRA_CONTINUE = 1,
    RAPIRA_RECYCLE = 2,
};

static zend_class_entry *rapira_plugin_info_ce;
static zend_class_entry *rapira_plugin_handler_config_ce;
static zend_class_entry *rapira_plugin_handler_ce;
static zend_class_entry *rapira_exception_ce;
static zend_class_entry *rapira_http_handler_config_ce;
static zend_class_entry *rapira_http_runtime_info_ce;
static zend_class_entry *rapira_http_handler_ce;

static zend_object_handlers rapira_no_ctor_handlers;

// Handlers for the classes only a factory may produce. object_init_ex never
// consults get_constructor, so the C factories below still work.
static zend_function *rapira_no_ctor(zend_object *object) {
    zend_throw_error(NULL, "Cannot directly construct %s",
                     ZSTR_VAL(object->ce->name));
    return NULL;
}

const zend_function_entry *rapira_php_functions(void) { return ext_functions; }

void rapira_register_classes(void) {
    rapira_plugin_info_ce = register_class_Rapira_PluginInfo();
    rapira_exception_ce =
        register_class_Rapira_RapiraException(zend_ce_exception);
    rapira_plugin_handler_config_ce = register_class_Rapira_PluginHandlerConfig();
    rapira_plugin_handler_ce = register_class_Rapira_PluginHandler();
    rapira_http_handler_config_ce =
        register_class_Rapira_Plugin_Http_HttpHandlerConfig(
            rapira_plugin_handler_config_ce);
    rapira_http_runtime_info_ce = register_class_Rapira_Plugin_Http_RuntimeInfo();
    rapira_http_handler_ce =
        register_class_Rapira_Plugin_Http_HttpHandler(rapira_plugin_handler_ce);

    rapira_no_ctor_handlers = *zend_get_std_object_handlers();
    rapira_no_ctor_handlers.get_constructor = rapira_no_ctor;
    // After registration on purpose: zend_do_inheritance rewrites the handler
    // wiring of any class registered with a parent (Zend/zend_inheritance.c).
    rapira_plugin_handler_ce->default_object_handlers = &rapira_no_ctor_handlers;
    rapira_http_handler_ce->default_object_handlers = &rapira_no_ctor_handlers;
    rapira_http_runtime_info_ce->default_object_handlers =
        &rapira_no_ctor_handlers;
}

/* Every property below is readonly, so the writes go through
zend_update_property* with the DECLARING class entry as scope. That sets
EG(fake_scope), which satisfies both the readonly first-write rule and the
protected(set) visibility `public readonly` auto-acquires (Zend/zend_API.c).
Only the first write per object succeeds, which is exactly what a factory
wants. */

ZEND_METHOD(Rapira_PluginInfo, __construct) {
    zend_string *name = NULL;
    zend_string *description = NULL;
    ZEND_PARSE_PARAMETERS_START(2, 2)
    Z_PARAM_STR(name)
    Z_PARAM_STR(description)
    ZEND_PARSE_PARAMETERS_END();

    zend_update_property_str(rapira_plugin_info_ce, Z_OBJ_P(ZEND_THIS), "name",
                             sizeof("name") - 1, name);
    zend_update_property_str(rapira_plugin_info_ce, Z_OBJ_P(ZEND_THIS),
                             "description", sizeof("description") - 1,
                             description);
}

ZEND_METHOD(Rapira_PluginHandlerConfig, __construct) {
    zval *info = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_OBJECT_OF_CLASS(info, rapira_plugin_info_ce)
    ZEND_PARSE_PARAMETERS_END();

    zend_update_property(rapira_plugin_handler_config_ce, Z_OBJ_P(ZEND_THIS),
                         "info", sizeof("info") - 1, info);
}

ZEND_METHOD(Rapira_Plugin_Http_HttpHandlerConfig, __construct) {
    zend_string *path_prefix = NULL;
    ZEND_PARSE_PARAMETERS_START(0, 1)
    Z_PARAM_OPTIONAL
    Z_PARAM_STR(path_prefix)
    ZEND_PARSE_PARAMETERS_END();

    // A prefix the front can never match would 404 every request with nothing to
    // point at; reject it here, where the script can still see why.
    if (path_prefix && ZSTR_LEN(path_prefix) > 0 && ZSTR_VAL(path_prefix)[0] != '/') {
        zend_throw_exception_ex(rapira_exception_ce, 0,
                                "pathPrefix must start with '/', got \"%s\"",
                                ZSTR_VAL(path_prefix));
        RETURN_THROWS();
    }

    // The capability slot this config targets, not the extension serving it.
    zval info;
    object_init_ex(&info, rapira_plugin_info_ce);
    zend_update_property_string(rapira_plugin_info_ce, Z_OBJ(info), "name",
                                sizeof("name") - 1, "http");
    zend_update_property_string(rapira_plugin_info_ce, Z_OBJ(info),
                                "description", sizeof("description") - 1,
                                "HTTP request handler");
    zend_update_property(rapira_plugin_handler_config_ce, Z_OBJ_P(ZEND_THIS),
                         "info", sizeof("info") - 1, &info);
    zval_ptr_dtor(&info);

    zend_update_property_str(rapira_http_handler_config_ce, Z_OBJ_P(ZEND_THIS),
                             "pathPrefix", sizeof("pathPrefix") - 1,
                             path_prefix ? path_prefix : ZSTR_EMPTY_ALLOC());
}

ZEND_METHOD(Rapira_Plugin_Http_HttpHandler, handleRequest) {
    zend_fcall_info fci;
    zend_fcall_info_cache fcc;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_FUNC(fci, fcc)
    ZEND_PARSE_PARAMETERS_END();

    // The fcc is deliberately not stored on the object: the call happens inside
    // this frame, so plain Z_PARAM_FUNC is correct and the class needs no get_gc.
    int action = rapira_rs_handle_request(&fci, &fcc);
    if (action == RAPIRA_RECYCLE) {
        // A teardown/handler bailout left the executor corrupt (imbalanced VM
        // stack, half-torn request). Unwind the whole resident script - over PHP
        // frames only, up to php_execute_script's zend_try - so no bytecode runs
        // over it; run_cycle then does the full php_request_shutdown + recycle.
        zend_bailout();
    }
    RETURN_BOOL(action == RAPIRA_CONTINUE);
}

ZEND_METHOD(Rapira_Plugin_Http_HttpHandler, getInfo) {
    ZEND_PARSE_PARAMETERS_NONE();

    rapira_runtime_info info;
    if (!rapira_rs_runtime_info(&info)) {
        zend_throw_exception(rapira_exception_ce,
                             "runtime info is unavailable on this thread", 0);
        RETURN_THROWS();
    }

    object_init_ex(return_value, rapira_http_runtime_info_ce);
    zend_class_entry *ce = rapira_http_runtime_info_ce;
    zend_object *obj = Z_OBJ_P(return_value);
    zend_update_property_string(ce, obj, "state", sizeof("state") - 1,
                                info.state);
    zend_update_property_long(ce, obj, "pid", sizeof("pid") - 1,
                              (zend_long)info.pid);
    zend_update_property_long(ce, obj, "queued", sizeof("queued") - 1,
                              (zend_long)info.queued);
    zend_update_property_long(ce, obj, "handled", sizeof("handled") - 1,
                              (zend_long)info.handled);
    zend_update_property_long(ce, obj, "errors", sizeof("errors") - 1,
                              (zend_long)info.errors);
    zend_update_property_long(ce, obj, "recycles", sizeof("recycles") - 1,
                              (zend_long)info.recycles);
    zend_update_property_long(ce, obj, "restarts", sizeof("restarts") - 1,
                              (zend_long)info.restarts);
}

ZEND_FUNCTION(Rapira_create_plugin_handler) {
    zval *config = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 1)
    Z_PARAM_OBJECT_OF_CLASS(config, rapira_plugin_handler_config_ce)
    ZEND_PARSE_PARAMETERS_END();

    if (!instanceof_function(Z_OBJCE_P(config), rapira_http_handler_config_ce)) {
        zend_throw_exception_ex(rapira_exception_ce, 0,
                                "no plugin handler for config %s",
                                ZSTR_VAL(Z_OBJCE_P(config)->name));
        RETURN_THROWS();
    }
    // Classic mode re-includes the script per request and never runs the
    // resident loop, so a handler there could only ever report shutdown.
    if (!rapira_rs_worker_mode()) {
        zend_throw_exception(rapira_exception_ce,
                             "plugin handlers require worker mode", 0);
        RETURN_THROWS();
    }

    // Hand the plugin its PHP-declared config as an opaque JSON blob; the plugin
    // owns the schema. The inherited `info` rides along; the plugin ignores fields
    // it does not know. A failed encode (a property holding invalid UTF-8) leaves
    // the fragment written so far in `buf` - the object encoder has already emitted
    // '{' and never emits the closing '}' - so shipping it would hand the plugin
    // invalid JSON, which it can only read as "nothing declared". Refuse instead.
    smart_str buf = {0};
    if (php_json_encode(&buf, config, 0) == FAILURE) {
        smart_str_free(&buf);
        zend_throw_exception_ex(rapira_exception_ce, 0,
                                "cannot serialize %s (invalid UTF-8 in a property?)",
                                ZSTR_VAL(Z_OBJCE_P(config)->name));
        RETURN_THROWS();
    }
    // The parameter is an object, so a successful encode wrote at least "{}".
    rapira_rs_set_handler_config((const uint8_t *)ZSTR_VAL(buf.s), ZSTR_LEN(buf.s));
    smart_str_free(&buf);

    object_init_ex(return_value, rapira_http_handler_ce);
    zend_update_property(rapira_plugin_handler_ce, Z_OBJ_P(return_value),
                         "config", sizeof("config") - 1, config);
}
