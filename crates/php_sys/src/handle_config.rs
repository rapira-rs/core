//! Rust half of the PHP plugin-handler classes (crates/php_sys/handle_config.c).
//!
//! Everything here reads state the worker thread already maintains; it owns none
//! of its own.

use std::cell::RefCell;
use std::ffi::c_char;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};

use rapira_scoreboard::{SLOT_ACTIVE, SLOT_DRAINING, SLOT_IDLE, SLOT_STARTING};

use crate::rapira_worker::worker_mode_active;
use crate::scoreboard::SB;
use crate::start::intake_depth;

/// Per-worker slot holding the JSON config blob PHP declared for its handler.
/// Written by the PHP thread at `create_plugin_handler`, read (cloned out) by the
/// extension thread through `RapiraHandle`. `None` until the script declares one.
/// The `Arc` is the only one: writer and handle share this one cell across threads.
pub(crate) type ConfigCell = Arc<Mutex<Option<Vec<u8>>>>;

thread_local! {
    // The PHP worker thread's clone of the cell, so the C writer can reach it.
    static WRITER: RefCell<Option<ConfigCell>> = const { RefCell::new(None) };
}

/// Install on the PHP worker thread before the first job (next to `quota::install`).
pub(crate) fn install(cell: ConfigCell) {
    WRITER.with_borrow_mut(|w| *w = Some(cell));
}

/// Hand the plugin the JSON config blob PHP declared at `create_plugin_handler`.
///
/// # Safety
/// Called from C on the PHP worker thread; `ptr`/`len` describe a valid byte range
/// that need not outlive the call (the bytes are copied here).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_set_handler_config(ptr: *const u8, len: usize) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: the caller guarantees `ptr`/`len` describe a readable byte range for
    // the duration of the call; `to_vec` copies before we return.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
    WRITER.with_borrow(|w| {
        if let Some(cell) = w {
            *cell.lock().expect("config cell poisoned") = Some(bytes);
        }
    });
}

/// Keep in sync with `rapira_runtime_info` in handle_config.h.
#[repr(C)]
pub struct RapiraRuntimeInfo {
    state: *const c_char,
    pid: u32,
    queued: u64,
    handled: u64,
    errors: u64,
    recycles: u64,
    restarts: u64,
}

fn state_name(state: u32) -> *const c_char {
    match state {
        SLOT_STARTING => c"starting".as_ptr(),
        SLOT_IDLE => c"idle".as_ptr(),
        SLOT_ACTIVE => c"active".as_ptr(),
        SLOT_DRAINING => c"draining".as_ptr(),
        _ => c"free".as_ptr(),
    }
}

/// True when the resident worker loop owns this thread; false under classic mode.
///
/// # Safety
/// Called from C (`Rapira\create_plugin_handler`) on the PHP worker thread.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_worker_mode() -> bool {
    worker_mode_active()
}

/// Fill `out` with this worker's live counters. `false` means no scoreboard slot
/// is installed on this thread, so there is nothing to report.
///
/// # Safety
/// Called from C (`HttpHandler::getInfo`) on the PHP worker thread; `out` must be
/// a valid, writable `rapira_runtime_info`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_runtime_info(out: *mut RapiraRuntimeInfo) -> bool {
    if out.is_null() {
        return false;
    }
    let Some(s) = SB.get() else { return false };

    let info = RapiraRuntimeInfo {
        state: state_name(s.state.load(Relaxed)),
        pid: s.pid.load(Relaxed),
        queued: intake_depth(),
        handled: s.handled.load(Relaxed),
        errors: s.errors.load(Relaxed),
        recycles: s.recycles.load(Relaxed),
        restarts: s.restarts.load(Relaxed),
    };
    // SAFETY: the caller guarantees `out` points at a writable
    // `rapira_runtime_info`; the struct is #[repr(C)] and layout-matched to it.
    unsafe { out.write(info) };
    true
}
