//! gRPC PHP-side entry-point glue.
//!
//! The job-pull and userland-presentation contract for the resident gRPC loop is
//! still under design: how a job's service/method, metadata, and raw message reach
//! PHP, and the recycle/bailout classification. This is the skeleton the C
//! `rapira_handle_grpc_request` links against; it stops the loop immediately until
//! that design lands.

#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_handle_grpc_request() -> bool {
    false
}
