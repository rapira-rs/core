//! Rust half of the PHP plugin-handler classes (crates/php_sys/plugin_handler.c):
//! the worker's live counters, and whether the resident loop owns this thread.

use std::ffi::c_char;
use std::sync::atomic::Ordering::Relaxed;

use rapira_scoreboard::{SLOT_ACTIVE, SLOT_DRAINING, SLOT_IDLE, SLOT_STARTING};

use crate::callbacks::guard;
use crate::rapira_worker::worker_mode_active;
use crate::scoreboard::SB;
use crate::start::intake_depth;

/// Keep in sync with `rapira_runtime_info` in plugin_handler.h.
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
/// Called from C (`Rapira\create_plugin_handler`) on the PHP worker thread.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_worker_mode() -> bool {
    guard(false, worker_mode_active)
}

/// Fill `out` with this worker's live counters. `false` means no scoreboard slot
/// is installed on this thread, so there is nothing to report.
///
/// # Safety
/// Called from C (`HttpHandler::getInfo`) on the PHP worker thread; `out` must be
/// a valid, writable `rapira_runtime_info`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_runtime_info(out: *mut RapiraRuntimeInfo) -> bool {
    guard(false, || {
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
    })
}
