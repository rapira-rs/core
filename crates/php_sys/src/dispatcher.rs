//! `\Rapira\get_version()` and `\Rapira\log()`: the version string, the
//! LogLevel mapping, and the context serialization (throwables flattened, then
//! `php_json_encode`). The C shells own only the ZPP layer.

use std::{
    ffi::{CStr, c_char, c_int},
    ptr::null_mut,
};

use tracing::event;

use crate::{
    HashPosition, HashTable, IS_OBJECT, PHP_JSON_PARTIAL_OUTPUT_ON_ERROR, add_assoc_stringl_ex,
    add_assoc_zval_ex, callbacks::guard, php_json_encode, rapira_array_init, rapira_smart_str_free,
    smart_str, zend, zend_ce_throwable, zend_class_entry, zend_get_exception_base,
    zend_hash_get_current_data_ex, zend_hash_get_current_key_ex, zend_hash_index_update,
    zend_hash_internal_pointer_reset_ex, zend_hash_move_forward_ex, zend_object,
    zend_read_property, zend_string, zval, zval_add_ref, zval_ptr_dtor,
};

/// # Safety
/// `len` must be a writable `usize`. The returned pointer is `'static`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_version(len: *mut usize) -> *const c_char {
    const VERSION: &CStr =
        match CStr::from_bytes_with_nul(concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes()) {
            Ok(v) => v,
            Err(_) => c"unknown",
        };

    guard(null_mut(), || {
        unsafe {
            len.write(VERSION.count_bytes());
        };
        VERSION.as_ptr()
    })
}

fn emit(level: c_int, message: &[u8], context: &[u8]) {
    let message = String::from_utf8_lossy(message);
    let context = String::from_utf8_lossy(context);

    macro_rules! log_at {
        ($lvl:expr) => {
            if context.is_empty() {
                event!(target: "app", $lvl, "{message}");
            } else {
                event!(target: "app", $lvl, context = %context, "{message}");
            }
        };
    }
    match level {
        0 => log_at!(tracing::Level::ERROR),
        1 => log_at!(tracing::Level::WARN),
        3 => log_at!(tracing::Level::DEBUG),
        4 => log_at!(tracing::Level::TRACE),
        _ => log_at!(tracing::Level::INFO),
    }
}

/// An enum case's name is its first property slot (php-src reads
/// `OBJ_PROP_NUM(zobj, 0)` in `zend_enum_fetch_case_name`, Zend/zend_enum.h).
/// # Safety
/// `level` a live `Rapira\LogLevel` case (ZPP-checked).
unsafe fn level_from_case(level: *mut zend_object) -> c_int {
    unsafe {
        let name_zv = (*level).properties_table.as_ptr();
        match zend::zstr_bytes((*name_zv).value.str_) {
            b"Error" => 0,
            b"Warning" => 1,
            b"Info" => 2,
            b"Debug" => 3,
            b"Trace" => 4,
            // a case added to the stub but not here must not vanish below the
            // configured log level: fail loud
            _ => 0,
        }
    }
}

/// Read a property and append a counted copy to `dst`.
/// # Safety
/// Frame rules (zend.rs): zvals and raw pointers only.
unsafe fn add_prop(
    dst: *mut zval,
    scope: *mut zend_class_entry,
    ex: *mut zend_object,
    name: &CStr,
) {
    unsafe {
        let mut rv: zval = std::mem::zeroed();
        let src = zend_read_property(scope, ex, name.as_ptr(), name.count_bytes(), true, &mut rv);
        let mut copy: zval = *src;
        zval_add_ref(&mut copy);
        add_assoc_zval_ex(dst, name.as_ptr(), name.count_bytes(), &mut copy);
    }
}

/// Throwable state lives in private props, invisible to json_encode: flatten
/// class/message/code/file/line explicitly, walking the previous-exception
/// chain down to `depth`.
/// # Safety
/// `ex` a live Throwable; frame rules (zend.rs).
unsafe fn flatten_throwable(dst: *mut zval, ex: *mut zend_object, depth: i32) {
    unsafe {
        rapira_array_init(dst, 6);
        let name = zend::zstr_bytes((*(*ex).ce).name);
        add_assoc_stringl_ex(
            dst,
            c"class".as_ptr(),
            c"class".count_bytes(),
            name.as_ptr().cast::<c_char>(),
            name.len(),
        );
        let base = zend_get_exception_base(ex);
        for prop in [c"message", c"code", c"file", c"line"] {
            add_prop(dst, base, ex, prop);
        }
        if depth <= 0 {
            return;
        }
        let mut rv: zval = std::mem::zeroed();
        let prev = zend_read_property(
            base,
            ex,
            c"previous".as_ptr(),
            c"previous".count_bytes(),
            true,
            &mut rv,
        );
        if !prev.is_null() && zend::zval_type(prev) == IS_OBJECT {
            let mut flat: zval = std::mem::zeroed();
            flatten_throwable(&mut flat, (*prev).value.obj, depth - 1);
            add_assoc_zval_ex(
                dst,
                c"previous".as_ptr(),
                c"previous".count_bytes(),
                &mut flat,
            );
        }
    }
}

/// Rebuild `context` with every Throwable value flattened (any key), leaving
/// the original untouched, and JSON-encode the result.
/// # Safety
/// `context` a live, non-empty array; frame rules (zend.rs) until the encode
/// completes — the returned Vec is created after the last bailing call.
unsafe fn context_json(context: *mut HashTable) -> Vec<u8> {
    unsafe {
        let mut rebuilt: zval = std::mem::zeroed();
        rapira_array_init(&mut rebuilt, (*context).nNumOfElements);
        let mut pos: HashPosition = 0;
        zend_hash_internal_pointer_reset_ex(context, &mut pos);
        loop {
            // raw pointer: the pos parameter is *mut on 8.4 and *const on 8.5
            let entry = zend_hash_get_current_data_ex(context, &raw mut pos);
            if entry.is_null() {
                break;
            }
            // see through a by-reference slot, or a referenced Throwable
            // would skip the flattener and encode as {}
            let entry = zend::deref(entry);
            let mut skey: *mut zend_string = null_mut();
            let mut nkey = 0;
            let kt = zend_hash_get_current_key_ex(context, &mut skey, &mut nkey, &pos);
            let mut out: zval = std::mem::zeroed();
            if zend::zval_type(entry) == IS_OBJECT
                && zend::instanceof((*(*entry).value.obj).ce, zend_ce_throwable)
            {
                flatten_throwable(&mut out, (*entry).value.obj, 4);
            } else {
                out = *entry;
                zval_add_ref(&mut out);
            }
            if i64::from(kt) == crate::HASH_KEY_IS_STRING && !skey.is_null() {
                let kb = zend::zstr_bytes(skey);
                // zend_strings are NUL-terminated, covering the symtable
                // prefilter's one-byte overread
                add_assoc_zval_ex(&mut rebuilt, (*skey).val.as_ptr(), kb.len(), &mut out);
            } else {
                zend_hash_index_update(rebuilt.value.arr, nkey, &mut out);
            }
            zend_hash_move_forward_ex(context, &mut pos);
        }

        let mut buf: smart_str = std::mem::zeroed();
        let _ = php_json_encode(
            &mut buf,
            &mut rebuilt,
            PHP_JSON_PARTIAL_OUTPUT_ON_ERROR as c_int,
        );
        // no bailing call below this line: owning Rust values are safe now
        let json = if buf.s.is_null() {
            Vec::new()
        } else {
            zend::zstr_bytes(buf.s).to_vec()
        };
        rapira_smart_str_free(&mut buf);
        zval_ptr_dtor(&mut rebuilt);
        json
    }
}

/// # Safety
/// `message` a live zend_string; `level` NULL or a LogLevel case; `context`
/// NULL or a live array — all ZPP-owned for the call. Engine active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_log_call(
    message: *mut zend_string,
    level: *mut zend_object,
    context: *mut HashTable,
) {
    // event! dispatches into the installed subscriber, which is arbitrary
    // code; an unwind out of an extern "C" frame aborts the process.
    guard((), || unsafe {
        let lvl = if level.is_null() {
            2
        } else {
            level_from_case(level)
        };
        let json = if context.is_null() || (*context).nNumOfElements == 0 {
            Vec::new()
        } else {
            context_json(context)
        };
        emit(lvl, zend::zstr_bytes(message), &json);
    })
}
