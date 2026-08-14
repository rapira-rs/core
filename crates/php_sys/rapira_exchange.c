#include "rapira_classes.h"
#include "wrapper.h"

#include "zend_API.h"
#include "zend_exceptions.h"

// rust glue; the verbs throw from Rust and report false with the throw pending
extern bool rapira_rs_exchange_build_request(rapira_exchange_obj *ex,
                                             zval *return_value);
extern bool rapira_rs_exchange_write_head(void *job, int64_t status,
                                          HashTable *headers);
extern bool rapira_rs_exchange_write_body(void *job, const char *p, size_t len,
                                          bool eos);
extern bool rapira_rs_exchange_is_finalized(const void *job);

ZEND_METHOD(Rapira_Internal_Http_Exchange, __construct) {
    zend_throw_error(NULL, "host-created");
}

static void *exchange_job(zval *this_ptr) {
    void *job = rapira_exchange_from(Z_OBJ_P(this_ptr))->job;
    if (job == NULL) {
        zend_throw_error(NULL, "exchange carries no host state");
    }
    return job;
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, writeHead) {
    zend_long status;
    HashTable *headers = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 2)
    Z_PARAM_LONG(status)
    Z_PARAM_OPTIONAL
    Z_PARAM_ARRAY_HT(headers)
    ZEND_PARSE_PARAMETERS_END();

    void *job = exchange_job(ZEND_THIS);
    if (job == NULL) {
        RETURN_THROWS();
    }
    if (!rapira_rs_exchange_write_head(job, (int64_t)status, headers)) {
        rapira_throw_or_backstop("writeHead");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, writeBody) {
    zend_string *content;
    bool eos = true;
    ZEND_PARSE_PARAMETERS_START(1, 2)
    Z_PARAM_STR(content)
    Z_PARAM_OPTIONAL
    Z_PARAM_BOOL(eos)
    ZEND_PARSE_PARAMETERS_END();

    void *job = exchange_job(ZEND_THIS);
    if (job == NULL) {
        RETURN_THROWS();
    }
    if (!rapira_rs_exchange_write_body(job, ZSTR_VAL(content),
                                       ZSTR_LEN(content), eos)) {
        rapira_throw_or_backstop("writeBody");
        RETURN_THROWS();
    }
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, isFinalized) {
    ZEND_PARSE_PARAMETERS_NONE();
    void *job = exchange_job(ZEND_THIS);
    if (job == NULL) {
        RETURN_THROWS();
    }
    RETURN_BOOL(rapira_rs_exchange_is_finalized(job));
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, isCancelled) {
    ZEND_PARSE_PARAMETERS_NONE();
    // Host-closed detection (deadline, gone client, drain) is not wired yet;
    // until it is, no unit is ever cancelled.
    RETURN_FALSE;
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, sendFile) {
    zend_throw_error(NULL, "sendFile() is not implemented");
    RETURN_THROWS();
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, writeTrailers) {
    zend_throw_error(NULL, "writeTrailers() is not implemented");
    RETURN_THROWS();
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, flush) {
    zend_throw_error(NULL, "flush() is not implemented");
    RETURN_THROWS();
}

// ---- getRequest: the graph builder lives in Rust (exchange.rs); this shell
// owns the macro layer only

ZEND_METHOD(Rapira_Internal_Http_Exchange, getRequest) {
    ZEND_PARSE_PARAMETERS_NONE();
    if (exchange_job(ZEND_THIS) == NULL) {
        RETURN_THROWS();
    }
    rapira_exchange_obj *ex = rapira_exchange_from(Z_OBJ_P(ZEND_THIS));
    if (!rapira_rs_exchange_build_request(ex, return_value)) {
        // the builder throws before returning false; a caught Rust panic is
        // the one path that cannot, so backstop it here
        if (!EG(exception)) {
            zend_throw_error(NULL, "request construction failed");
        }
        RETURN_THROWS();
    }
}
