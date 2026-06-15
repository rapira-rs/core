mod types;
mod module;
mod callbacks;

include!(concat!(env!("OUT_DIR"), "/bindings.rs")); // all bindgen types/fns/consts

// wrapper.c globals accessors + the module.c module entry.
unsafe extern "C" {
    pub fn rapira_sg() -> *mut sapi_globals_struct;
    pub fn rapira_eg() -> *mut zend_executor_globals;
    pub fn rapira_pg() -> *mut php_core_globals;
    pub fn rapira_cg() -> *mut zend_compiler_globals;
    pub static mut rapira_module_entry: zend_module_entry; // from module.c
}
