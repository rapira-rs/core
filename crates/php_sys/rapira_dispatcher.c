#include "rapira_classes.h"

#include "ext/json/php_json.h"
#include "wrapper.h"
#include "zend.h"
#include "zend_operators.h"
#include "zend_portability.h"
#include "zend_ptr_stack.h"
#include "zend_smart_str.h"

#include "zend_API.h"
#include "zend_enum.h"
#include "zend_hash.h"
#include "zend_string.h"

// rust glue
extern const char *rapira_rs_version(size_t *len);
extern void rapira_rs_log(const char *msg, size_t msg_len, int level,
                          const char *ctx, size_t ctx_len);
extern int rapira_rs_receive(int64_t timeout_us, void **out_job);
extern int rapira_rs_try_receive(void **out_job);
extern void rapira_rs_dispatcher_counters(int64_t *pending, int64_t *active);

// rapira mode
int rapira_mode = RAPIRA_MODE_CLASSIC;

enum {
    RAPIRA_LOG_ERROR = 0,
    RAPIRA_LOG_WARN,
    RAPIRA_LOG_INFO,
    RAPIRA_LOG_DEBUG,
    RAPIRA_LOG_TRACE,
};

// zend_read_property borrows the value, so to satisfy ownership, we copy it
// this method is a shortland for reading a property and copying its value into
// a zval
static void add_property(zval *dst, zend_class_entry *scope, zend_object *ex,
                         const char *name, size_t len) {
    zval rv, copy;
    ZVAL_COPY(&copy, zend_read_property(scope, ex, name, len, 1, &rv));
    add_assoc_zval_ex(dst, name, len, &copy);
}

// this is a special case for throwable objects.
// they have a private fields, which if passed in context (log),
// we'll lose them, since they are private (and we're using json)
static void throwable(zval *dst, zend_object *ex, int depth) {
    zend_class_entry *base = zend_get_exception_base(ex);
    array_init(dst);

    add_assoc_str(dst, "class", zend_string_copy(ex->ce->name));
    add_property(dst, base, ex, ZEND_STRL("message"));
    add_property(dst, base, ex, ZEND_STRL("code"));
    add_property(dst, base, ex, ZEND_STRL("file"));
    add_property(dst, base, ex, ZEND_STRL("line"));

    // depth bounds the walk down the previous-exception chain
    if (depth <= 0) {
        return;
    }

    zval rv;
    zval *prev = zend_read_property(base, ex, ZEND_STRL("previous"), true, &rv);
    if (prev != NULL && Z_TYPE_P(prev) == IS_OBJECT) {
        zval flat;
        throwable(&flat, Z_OBJ_P(prev), depth - 1);
        add_assoc_zval_ex(dst, ZEND_STRL("previous"), &flat);
    }
}

// thread local static macro
// tls here is thread-local storage, not networking term
ZEND_TLS zval rapira_dispatcher_instance;

void rapira_dispatcher_release(void) {
    if (!Z_ISUNDEF(rapira_dispatcher_instance)) {
        zval_ptr_dtor(&rapira_dispatcher_instance);
        ZVAL_UNDEF(&rapira_dispatcher_instance);
    }
}

ZEND_FUNCTION(Rapira_get_version) {
    ZEND_PARSE_PARAMETERS_NONE();

    size_t len = 0;
    const char *version = rapira_rs_version(&len);
    RETURN_STRINGL(version, len);
}

ZEND_FUNCTION(Rapira_get_dispatcher) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (rapira_mode != RAPIRA_MODE_DISPATCHER) {
        zend_throw_error(
            rapira_ce_not_in_worker_mode_error,
            "nothing dispatches work to this process outside worker mode");
        RETURN_THROWS();
    }

    if (Z_ISUNDEF(rapira_dispatcher_instance)) {
        object_init_ex(&rapira_dispatcher_instance,
                       rapira_ce_internal_http_dispatcher);
    }

    RETURN_COPY(&rapira_dispatcher_instance);
}

static int level_from_case(zend_object *level) {
    zend_string *name = Z_STR_P(zend_enum_fetch_case_name(level));

    if (zend_string_equals_literal(name, "Error")) {
        return RAPIRA_LOG_ERROR;
    }
    if (zend_string_equals_literal(name, "Warning")) {
        return RAPIRA_LOG_WARN;
    }
    if (zend_string_equals_literal(name, "Info")) {
        return RAPIRA_LOG_INFO;
    }
    if (zend_string_equals_literal(name, "Debug")) {
        return RAPIRA_LOG_DEBUG;
    }
    if (zend_string_equals_literal(name, "Trace")) {
        return RAPIRA_LOG_TRACE;
    }

    return RAPIRA_LOG_INFO;
}

ZEND_FUNCTION(Rapira_log) {
    zend_string *message = NULL;
    zval *level = NULL;
    HashTable *context = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 3)
    Z_PARAM_STR(message)
    Z_PARAM_OPTIONAL
    // log level is enum in PHP, so we use an object of the log level class
    // it is also optional
    Z_PARAM_OBJECT_OF_CLASS(level, rapira_ce_log_level)
    Z_PARAM_ARRAY_HT(context)
    ZEND_PARSE_PARAMETERS_END();

    int lvl = level ? level_from_case(Z_OBJ_P(level)) : RAPIRA_LOG_INFO;
    smart_str json = {0};

    if (context != NULL && zend_hash_num_elements(context) > 0) {
        zval tmp;
        // dup to avoid modifying the original context
        ZVAL_ARR(&tmp, zend_array_dup(context));

        zval *val = NULL;
        ZEND_HASH_FOREACH_VAL(Z_ARRVAL(tmp), val) {
            if (Z_TYPE_P(val) == IS_OBJECT &&
                instanceof_function(Z_OBJCE_P(val), zend_ce_throwable)) {
                zval flat;
                throwable(&flat, Z_OBJ_P(val), 4);
                zval_ptr_dtor(val);
                ZVAL_COPY_VALUE(val, &flat);
            }
        }
        ZEND_HASH_FOREACH_END();

        php_json_encode(&json, &tmp, PHP_JSON_PARTIAL_OUTPUT_ON_ERROR);
        smart_str_0(&json);
        zval_ptr_dtor(&tmp);
    }

    rapira_rs_log(ZSTR_VAL(message), ZSTR_LEN(message), lvl,
                  json.s ? ZSTR_VAL(json.s) : NULL,
                  json.s ? ZSTR_LEN(json.s) : 0);
    smart_str_free(&json);
}

ZEND_METHOD(Rapira_Internal_Http_Dispatcher, name) {
    ZEND_PARSE_PARAMETERS_NONE();
    // the plugin's root TOML section
    RETURN_STRING("http");
}

ZEND_METHOD(Rapira_Internal_Http_Dispatcher, __construct) {
    zend_throw_error(NULL,
                     "host-created; obtain it from \\Rapira\\get_dispatcher()");
}

ZEND_METHOD(Rapira_Internal_Http_DispatcherInfo, __construct) {
    zend_throw_error(NULL, "host-created");
}

static void throw_recv(int rc) {
    switch (rc) {
    case RAPIRA_RECV_TIMEOUT:
        zend_throw_exception(rapira_ce_timeout_exception,
                             "no work became available within the timeout", 0);
        return;
    case RAPIRA_RECV_CLOSED:
        zend_throw_exception(rapira_ce_closed_exception,
                             "no more work will ever arrive", 0);
        return;
    case RAPIRA_RECV_BUSY:
    default:
        zend_throw_error(NULL, "receive() while a Rapira\\Http\\Exchange is "
                               "unfinalized; finalize it first");
    }
}

// The wrapping object exists before the receive: zend_object_alloc can bailout,
// and a Box already handed to C with no owner would leak its Frame sender and
// hang the client. With the object first, the error path is a plain dtor and
// free_obj no-ops on the NULL job slot. The wall-timer disarm/re-arm lives on
// the Rust side (exchange.rs), sequenced after the single-flight check.
ZEND_METHOD(Rapira_Internal_Http_Dispatcher, receive) {
    zend_long timeout = -1;
    ZEND_PARSE_PARAMETERS_START(0, 1)
    Z_PARAM_OPTIONAL
    Z_PARAM_LONG(timeout)
    ZEND_PARSE_PARAMETERS_END();
    if (timeout < -1) {
        zend_argument_value_error(1, "must be greater than or equal to -1");
        RETURN_THROWS();
    }

    object_init_ex(return_value, rapira_ce_internal_http_exchange);
    void *job = NULL;
    int rc = rapira_rs_receive((int64_t)timeout, &job);
    if (rc != RAPIRA_RECV_OK) {
        zval_ptr_dtor(return_value);
        ZVAL_UNDEF(return_value);
        throw_recv(rc);
        RETURN_THROWS();
    }
    rapira_exchange_from(Z_OBJ_P(return_value))->job = job;
}

ZEND_METHOD(Rapira_Internal_Http_Dispatcher, tryReceive) {
    ZEND_PARSE_PARAMETERS_NONE();
    object_init_ex(return_value, rapira_ce_internal_http_exchange);
    void *job = NULL;
    int rc = rapira_rs_try_receive(&job);
    if (rc == RAPIRA_RECV_OK) {
        rapira_exchange_from(Z_OBJ_P(return_value))->job = job;
        return;
    }
    zval_ptr_dtor(return_value);
    ZVAL_UNDEF(return_value);
    if (rc == RAPIRA_RECV_EMPTY) {
        RETURN_NULL();
    }
    throw_recv(rc);
    RETURN_THROWS();
}

ZEND_METHOD(Rapira_Internal_Http_Dispatcher, getInfo) {
    ZEND_PARSE_PARAMETERS_NONE();
    int64_t pending = 0, active = 0;
    rapira_rs_dispatcher_counters(&pending, &active);
    object_init_ex(return_value, rapira_ce_internal_http_dispatcher_info);
    rapira_dispatcher_info_obj *info =
        rapira_dispatcher_info_from(Z_OBJ_P(return_value));
    info->pending = (zend_long)pending;
    info->active = (zend_long)active;
}

ZEND_METHOD(Rapira_Internal_Http_DispatcherInfo, pendingCount) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_LONG(rapira_dispatcher_info_from(Z_OBJ_P(ZEND_THIS))->pending);
}

ZEND_METHOD(Rapira_Internal_Http_DispatcherInfo, activeCount) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_LONG(rapira_dispatcher_info_from(Z_OBJ_P(ZEND_THIS))->active);
}
