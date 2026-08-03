#include "rapira_classes.h"

#include "ext/json/php_json.h"
#include "zend_smart_str.h"

#include "zend_API.h"
#include "zend_enum.h"
#include "zend_hash.h"

// rust glue
extern const char *rapira_rs_version(size_t *len);
extern void rapira_rs_log(const char *msg, size_t msg_len, int level,
                          const char *ctx, size_t ctx_len);

enum {
    RAPIRA_LOG_ERROR = 0,
    RAPIRA_LOG_WARN,
    RAPIRA_LOG_INFO,
    RAPIRA_LOG_DEBUG,
    RAPIRA_LOG_TRACE,
};

ZEND_FUNCTION(Rapira_get_version) {
    ZEND_PARSE_PARAMETERS_NONE();

    size_t len = 0;
    const char *version = rapira_rs_version(&len);
    RETURN_STRINGL(version, len);
}

ZEND_FUNCTION(Rapira_get_dispatcher) {
    ZEND_PARSE_PARAMETERS_NONE();
    zend_throw_error(NULL, "no dispatcher");
    RETURN_THROWS();
}

static int level_from_case(zend_object *level) {
    zend_string *name = Z_STR_P(zend_enum_fetch_case_name(level));

    if (zend_string_equals_literal(name, "Error")) {
        return RAPIRA_LOG_ERROR;
    }
    if (zend_string_equals_literal(name, "Warninig")) {
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
        ZVAL_ARR(&tmp, context);
        php_json_encode(&json, &tmp, PHP_JSON_PARTIAL_OUTPUT_ON_ERROR);
        smart_str_0(&json);
    }

    rapira_rs_log(ZSTR_VAL(message), ZSTR_LEN(message), lvl,
                  json.s ? ZSTR_VAL(json.s) : NULL,
                  json.s ? ZSTR_LEN(json.s) : 0);
    smart_str_free(&json);
}
