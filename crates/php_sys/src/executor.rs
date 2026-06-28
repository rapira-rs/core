use std::{ffi::CString, path::Path};

use crate::{
    php_execute_script, zend_destroy_file_handle, zend_file_handle, zend_stream_init_filename,
};

/// # Safety
/// Must be called on a thread with an initialized PHP interpreter (a live
/// `ts_resource`) inside an active request (between `php_request_startup` and
/// `php_request_shutdown`). Executes arbitrary PHP via `php_execute_script` and may
/// `zend_bailout`, so the caller must establish a `zend_try` boundary where required.
pub unsafe fn run_script(script: &Path) -> bool {
    unsafe {
        let c_script: CString =
            CString::new(script.to_string_lossy().as_bytes()).unwrap_or_default();
        let mut fh: zend_file_handle = std::mem::zeroed();
        zend_stream_init_filename(&mut fh, c_script.as_ptr());
        fh.primary_script = true;
        let ok: bool = php_execute_script(&mut fh);
        zend_destroy_file_handle(&mut fh);
        ok
    }
}
