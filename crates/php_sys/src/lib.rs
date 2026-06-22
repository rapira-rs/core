#[allow(clippy::all)]
pub mod bindings;

pub mod boot;
pub mod callbacks;
pub mod classic_worker;
pub mod context;
pub mod dispatcher;
pub mod executor;
pub mod module;
pub mod rapira_worker;
pub mod types;

pub use bindings::*;
pub use boot::Rapira;
pub use dispatcher::RapiraHandle;
pub use types::{Context, Frame, Mode, Request, ResponseHead};

use std::os::raw::c_int;

unsafe extern "C" {
    pub fn rapira_sg() -> *mut sapi_globals_struct;
    pub fn rapira_eg() -> *mut zend_executor_globals;
    pub fn rapira_pg() -> *mut php_core_globals;
    pub fn rapira_cg() -> *mut zend_compiler_globals;

    pub fn rapira_run_handler(fci: *mut zend_fcall_info, fcc: *mut zend_fcall_info_cache) -> c_int; //enum

    pub static mut rapira_module_entry: zend_module_entry; // from module.c
}
