#include "wrapper.h"

// Rust glue (crates/php_sys/src/grpc.rs). The job-pull and userland-presentation
// contract for the resident gRPC loop is still under design; this entry point is a
// registered skeleton and the Rust side stops the loop immediately until it lands.
extern bool rapira_rs_handle_grpc_request(void);

// PHP userland: `while (rapira_handle_grpc_request()) {}`. No args, returns bool.
PHP_FUNCTION(rapira_handle_grpc_request) {
    ZEND_PARSE_PARAMETERS_NONE();
    RETURN_BOOL(rapira_rs_handle_grpc_request());
}
