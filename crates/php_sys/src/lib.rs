#[allow(clippy::all)]
pub mod bindings;

pub mod callbacks;
pub mod classic_worker;
pub mod context;
pub mod executor;
pub mod grpc;
pub mod handler;
pub mod module;
pub mod quota;
pub mod rapira_worker;
pub mod scoreboard;
pub mod start;
pub mod types;

use std::ffi::c_int;

pub use bindings::*;
pub use handler::RapiraHandle;
pub use quota::WorkerHooks;
pub use start::{PhpModule, Rapira};
pub use types::{Context, Frame, Mode, Request, ResponseHead, StreamState};

// Zend SUCCESS/FAILURE differ across php-src versions, so they are hardcoded here rather
// than bound from the headers.
pub const SUCCESS: c_int = 0;
pub const FAILURE: c_int = -1;

// The Outcome-typed shims return a C `int`; the discriminant is validated at the call sites via
// `Outcome::from_c` (unexpected values fall back to `Bailout`) rather than transmuted here.
unsafe extern "C" {
    pub fn rapira_sg() -> *mut sapi_globals_struct;
    pub fn rapira_pg() -> *mut php_core_globals;
    pub fn rapira_finish_output() -> c_int;
    pub fn rapira_init_call_stack();
    pub fn rapira_clear_last_error();
    pub fn rapira_request_teardown() -> c_int;
    pub fn rapira_process_init();
    pub fn rapira_child_init();
    pub fn rapira_release_temporary_streams();
    pub fn rapira_request_activate() -> c_int;
    pub fn rapira_request_shutdown() -> c_int;
    // C shim (module.c) over rapira_rs_ub_write; raises the client-abort bailout from
    // C so the longjmp doesn't cross the Rust catch_unwind frame.
    // https://man7.org/linux/man-pages/man3/setjmp.3.html
    // https://doc.rust-lang.org/std/panic/fn.catch_unwind.html
    pub fn rapira_ub_write(str_: *const std::os::raw::c_char, len: usize) -> usize;

    pub fn rapira_run_handler(fci: *mut zend_fcall_info, fcc: *mut zend_fcall_info_cache) -> c_int;

    pub static mut rapira_module_entry: zend_module_entry; // from module.c
}
