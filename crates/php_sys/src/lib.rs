#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code
)]
pub mod boot;
pub mod callbacks;
pub mod classic_worker;
pub mod context;
pub mod dispatcher;
pub mod executor;
pub mod module;
pub mod rapira_worker;
pub mod types;

use std::os::raw::c_int;

pub use boot::Rapira;
pub use dispatcher::RapiraHandle;
pub use types::{Context, Frame, Mode, Request, ResponseHead};

include!(concat!(env!("OUT_DIR"), "/bindings.rs")); // bindgen types/fns/consts

unsafe extern "C" {
    pub fn rapira_sg() -> *mut sapi_globals_struct;
    pub fn rapira_eg() -> *mut zend_executor_globals;
    pub fn rapira_pg() -> *mut php_core_globals;
    pub fn rapira_cg() -> *mut zend_compiler_globals;

    pub fn rapira_run_handler(fci: *mut zend_fcall_info, fcc: *mut zend_fcall_info_cache) -> c_int; //enum

    pub static mut rapira_module_entry: zend_module_entry; // from module.c
}
