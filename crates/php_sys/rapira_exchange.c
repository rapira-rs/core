#include "rapira_classes.h"
#include "wrapper.h"

#include "zend_API.h"
#include "zend_exceptions.h"

// rust glue
extern void rapira_rs_exchange_request(const void *job, rapira_request_view *out);
extern void rapira_rs_exchange_header(const void *job, size_t i, rapira_str *name,
                                      rapira_str *value);
extern int rapira_rs_exchange_write_head(void *job, uint16_t status,
                                         const rapira_str *pairs, size_t npairs);
extern int rapira_rs_exchange_write_body(void *job, const char *p, size_t len,
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

// verb-code -> exception; ok returns false (nothing thrown)
static bool throw_verb(int rc) {
    switch (rc) {
    case RAPIRA_VERB_OK:
    case RAPIRA_VERB_INTERIM: // advisory 1xx head, dropped by design
        return false;
    case RAPIRA_VERB_FINALIZED:
        zend_throw_exception(rapira_ce_already_finalized_error,
                             "the response already ended", 0);
        return true;
    case RAPIRA_VERB_HEAD_WRITTEN:
        zend_throw_exception(rapira_ce_http_head_already_written_error,
                             "the final head has already been written", 0);
        return true;
    case RAPIRA_VERB_OVERFLOW:
        // the unit was sealed as truncated; the worker is not wedged
        zend_throw_error(NULL, "response exceeds the host buffer cap");
        return true;
    default:
        zend_throw_error(NULL, "exchange verb failed");
        return true;
    }
}

// RFC 9110 §5.6.2 tchar. A non-token name would pass a weaker check and then
// be dropped silently downstream instead of raising the promised ValueError.
// https://www.rfc-editor.org/rfc/rfc9110#section-5.6.2
static bool wire_token(const char *p, size_t len) {
    for (size_t i = 0; i < len; i++) {
        unsigned char c = (unsigned char)p[i];
        if (c <= 0x20 || c >= 0x7f || strchr("\"(),/:;<=>?@[\\]{}", c)) {
            return false;
        }
    }
    return true;
}

static bool wire_value(const char *p, size_t len) {
    for (size_t i = 0; i < len; i++) {
        if (p[i] == '\r' || p[i] == '\n' || p[i] == '\0') {
            return false;
        }
    }
    return true;
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, writeHead) {
    zend_long status;
    HashTable *headers = NULL;
    ZEND_PARSE_PARAMETERS_START(1, 2)
    Z_PARAM_LONG(status)
    Z_PARAM_OPTIONAL
    Z_PARAM_ARRAY_HT(headers)
    ZEND_PARSE_PARAMETERS_END();

    if (status < 100 || status > 599) {
        zend_value_error("status must be between 100 and 599, %" PRId64 " given",
                         (int64_t)status);
        RETURN_THROWS();
    }
    void *job = exchange_job(ZEND_THIS);
    if (job == NULL) {
        RETURN_THROWS();
    }

    // flatten array<non-empty-string, list<string>> into (name,value) pairs
    size_t npairs = 0;
    rapira_str *pairs = NULL;
    if (headers != NULL && zend_hash_num_elements(headers) > 0) {
        size_t cap = 0;
        zval *lv;
        ZEND_HASH_FOREACH_VAL(headers, lv) {
            ZVAL_DEREF(lv);
            if (Z_TYPE_P(lv) != IS_ARRAY) {
                zend_value_error("each header entry must be a list of strings");
                RETURN_THROWS();
            }
            cap += zend_hash_num_elements(Z_ARRVAL_P(lv));
        }
        ZEND_HASH_FOREACH_END();

        pairs = safe_emalloc(cap ? cap : 1, 2 * sizeof(rapira_str), 0);
        zend_string *name;
        ZEND_HASH_FOREACH_STR_KEY_VAL(headers, name, lv) {
            ZVAL_DEREF(lv);
            if (name == NULL || ZSTR_LEN(name) == 0 ||
                !wire_token(ZSTR_VAL(name), ZSTR_LEN(name))) {
                efree(pairs);
                zend_value_error("header name is not representable on the wire");
                RETURN_THROWS();
            }
            zval *item;
            ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(lv), item) {
                ZVAL_DEREF(item);
                if (Z_TYPE_P(item) != IS_STRING ||
                    !wire_value(Z_STRVAL_P(item), Z_STRLEN_P(item))) {
                    efree(pairs);
                    zend_value_error(
                        "header value is not representable on the wire");
                    RETURN_THROWS();
                }
                pairs[2 * npairs] = (rapira_str){ZSTR_VAL(name), ZSTR_LEN(name)};
                pairs[2 * npairs + 1] =
                    (rapira_str){Z_STRVAL_P(item), Z_STRLEN_P(item)};
                npairs++;
            }
            ZEND_HASH_FOREACH_END();
        }
        ZEND_HASH_FOREACH_END();
    }

    int rc = rapira_rs_exchange_write_head(job, (uint16_t)status, pairs, npairs);
    if (pairs != NULL) {
        efree(pairs);
    }
    if (throw_verb(rc)) {
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
    int rc = rapira_rs_exchange_write_body(job, ZSTR_VAL(content),
                                           ZSTR_LEN(content), eos);
    if (throw_verb(rc)) {
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

// ---- getRequest: build Rapira\Http\Request from the view, cache on the object

static void build_address(zval *dst, rapira_str ip, int32_t port) {
    if (port > 0) {
        object_init_ex(dst, rapira_ce_inet_address);
        zend_update_property_stringl(rapira_ce_inet_address, Z_OBJ_P(dst),
                                     ZEND_STRL("ip"), ip.ptr, ip.len);
        zend_update_property_long(rapira_ce_inet_address, Z_OBJ_P(dst),
                                  ZEND_STRL("port"), port);
    } else {
        // not an IP endpoint as far as the host can tell; the host does not
        // yet pass the accepting socket's real address through
        object_init_ex(dst, rapira_ce_unix_address);
        zend_update_property_null(rapira_ce_unix_address, Z_OBJ_P(dst),
                                  ZEND_STRL("path"));
    }
}

ZEND_METHOD(Rapira_Internal_Http_Exchange, getRequest) {
    ZEND_PARSE_PARAMETERS_NONE();
    rapira_exchange_obj *ex = rapira_exchange_from(Z_OBJ_P(ZEND_THIS));
    if (ex->job == NULL) {
        zend_throw_error(NULL, "exchange carries no host state");
        RETURN_THROWS();
    }
    if (!Z_ISUNDEF(ex->request)) {
        RETURN_COPY(&ex->request);
    }

    rapira_request_view v = {0};
    rapira_rs_exchange_request(ex->job, &v);

    zval headers;
    array_init_size(&headers, (uint32_t)v.header_count);
    for (size_t i = 0; i < v.header_count; i++) {
        rapira_str name = {0}, value = {0};
        rapira_rs_exchange_header(ex->job, i, &name, &value);
        if (name.ptr == NULL) { // caught panic on the Rust side: skip the entry
            continue;
        }
        zval list;
        array_init_size(&list, 1);
        add_next_index_stringl(&list, value.ptr, value.len);
        // symtable: an all-digit field name must land as an integer key or the
        // array disagrees with every userland lookup of it
        zend_symtable_str_update(Z_ARRVAL(headers), name.ptr, name.len, &list);
    }

    zval remote, server;
    build_address(&remote, v.remote_ip, v.remote_port);
    build_address(&server, v.server_ip, v.server_port);

    zval req;
    object_init_ex(&req, rapira_ce_http_request);
    zend_object *o = Z_OBJ(req);
    zend_class_entry *ce = rapira_ce_http_request;
    zend_update_property_stringl(ce, o, ZEND_STRL("method"), v.method.ptr,
                                 v.method.len);
    zend_update_property_stringl(ce, o, ZEND_STRL("uri"), v.uri.ptr, v.uri.len);
    zend_update_property_stringl(ce, o, ZEND_STRL("target"), v.target.ptr,
                                 v.target.len);
    if (v.authority.ptr != NULL) {
        zend_update_property_stringl(ce, o, ZEND_STRL("authority"),
                                     v.authority.ptr, v.authority.len);
    } else {
        zend_update_property_null(ce, o, ZEND_STRL("authority"));
    }
    zend_update_property_stringl(ce, o, ZEND_STRL("protocol"), v.protocol.ptr,
                                 v.protocol.len);
    zend_update_property(ce, o, ZEND_STRL("headers"), &headers);
    zend_update_property_stringl(ce, o, ZEND_STRL("body"), v.body.ptr,
                                 v.body.len);
    zend_update_property(ce, o, ZEND_STRL("remote"), &remote);
    zend_update_property(ce, o, ZEND_STRL("server"), &server);
    zend_update_property_null(ce, o, ZEND_STRL("tls"));
    zend_update_property_double(ce, o, ZEND_STRL("receivedAt"), v.received_at);

    zval_ptr_dtor(&headers);
    zval_ptr_dtor(&remote);
    zval_ptr_dtor(&server);

    ZVAL_COPY_VALUE(&ex->request, &req);
    RETURN_COPY(&ex->request);
}
