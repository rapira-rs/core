#[allow(clippy::all)]
pub mod bindings;

pub mod callbacks;
pub mod classic_worker;
pub mod context;
pub mod executor;
pub mod handler;
pub mod module;
pub mod rapira_worker;
pub mod scoreboard;
pub mod start;
pub mod types;

use std::ffi::c_int;

pub use bindings::*;
pub use handler::RapiraHandle;
pub use start::Rapira;
pub use types::{Context, Frame, Mode, Request, ResponseHead, StreamState};

// Zend status codes, which are different on master and 8.5 for example.
pub const SUCCESS: c_int = 0;
pub const FAILURE: c_int = -1;

unsafe extern "C" {
    pub fn rapira_sg() -> *mut sapi_globals_struct;
    pub fn rapira_eg() -> *mut zend_executor_globals;
    pub fn rapira_pg() -> *mut php_core_globals;
    pub fn rapira_cg() -> *mut zend_compiler_globals;
    pub fn rapira_finish_output() -> types::Outcome;
    pub fn rapira_init_call_stack();
    pub fn rapira_tsrmls_cache_update();
    pub fn rapira_clear_last_error();
    pub fn rapira_request_teardown() -> types::Outcome; //enum
    pub fn rapira_process_init();
    pub fn rapira_release_temporary_streams();
    pub fn rapira_request_activate() -> types::Outcome;
    pub fn rapira_request_shutdown() -> types::Outcome;
    // C shim (module.c) over rapira_rs_ub_write; raises the client-abort bailout from
    // C so the longjmp doesn't cross the Rust catch_unwind frame.
    pub fn rapira_ub_write(str_: *const std::os::raw::c_char, len: usize) -> usize;

    pub fn rapira_run_handler(
        fci: *mut zend_fcall_info,
        fcc: *mut zend_fcall_info_cache,
    ) -> types::Outcome; //enum

    pub static mut rapira_module_entry: zend_module_entry; // from module.c
}
