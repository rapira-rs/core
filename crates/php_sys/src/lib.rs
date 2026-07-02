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

pub use bindings::*;
pub use handler::RapiraHandle;
pub use start::Rapira;
pub use types::{Context, Frame, Mode, Request, ResponseHead};

// Zend status codes, which are different on master and 8.5 for example.
pub const SUCCESS: core::ffi::c_int = 0;
pub const FAILURE: core::ffi::c_int = -1;

unsafe extern "C" {
    pub fn rapira_sg() -> *mut sapi_globals_struct;
    pub fn rapira_eg() -> *mut zend_executor_globals;
    pub fn rapira_pg() -> *mut php_core_globals;
    pub fn rapira_cg() -> *mut zend_compiler_globals;
    pub fn rapira_finish_output() -> types::Outcome;
    pub fn rapira_init_call_stack();
    pub fn rapira_clear_last_error();
    pub fn rapira_activate_auto_globals();
    pub fn rapira_request_teardown() -> types::Outcome; //enum
    pub fn rapira_process_init();

    pub fn rapira_run_handler(
        fci: *mut zend_fcall_info,
        fcc: *mut zend_fcall_info_cache,
    ) -> types::Outcome; //enum

    pub static mut rapira_module_entry: zend_module_entry; // from module.c
}
