//! Post-fork worker body: signal setup, quota hooks, extension host + the
//! single PHP interpreter, and the exit-code protocol the master consumes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering::SeqCst};
use std::sync::{Arc, OnceLock};

use extension_host::{ExtensionHost, Stopper};
use php_sys::{Mode, Rapira, WorkerHooks};
use rapira_master::{WORKER_EXIT_RECYCLE, WORKER_EXIT_UNHEALTHY, WorkerEnv};

/// First writer wins; -1 = unset (drained → 0).
static WORKER_EXIT: AtomicI32 = AtomicI32::new(-1);

fn request_worker_exit(code: i32, stopper: &OnceLock<Stopper>) {
    if WORKER_EXIT
        .compare_exchange(-1, code, SeqCst, SeqCst)
        .is_ok()
        && let Some(s) = stopper.get()
    {
        s.stop();
    }
    // If this raced ahead of the stopper registration, the boot path re-checks
    // WORKER_EXIT right after registering and stops immediately.
}

/// Effective per-worker quota: max + rand(1..=grace), grace = max/2 (jitter so
/// a pool never recycles in lockstep). Entropy mixes pid + time
/// because a hash seed inherited from the pre-fork master would be identical
/// in every child.
fn effective_quota(max_requests: u64) -> u64 {
    if max_requests == 0 {
        return 0;
    }
    let grace = (max_requests / 2).max(1);
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    h.write_u128(
        std::time::UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    max_requests + 1 + (h.finish() % grace)
}

/// The entire post-fork worker body; returns the process exit code for the
/// master's fork bracket to `_exit` with. Never runs PHP module teardown —
/// workers exit and leave MSHUTDOWN to the master, which owns the single
/// engine teardown.
pub fn worker_body(
    env: WorkerEnv,
    host: ExtensionHost,
    mode: Mode,
    script: PathBuf,
    max_requests: u64,
) -> i32 {
    extension_host::arm_worker_signals();

    let stopper: Arc<OnceLock<Stopper>> = Arc::new(OnceLock::new());
    let hooks = WorkerHooks {
        max_requests: effective_quota(max_requests),
        on_quota: Some(Box::new({
            let stopper = stopper.clone();
            move || request_worker_exit(WORKER_EXIT_RECYCLE, &stopper)
        })),
        on_unhealthy: Some(Box::new({
            let stopper = stopper.clone();
            move || request_worker_exit(WORKER_EXIT_UNHEALTHY, &stopper)
        })),
        slot: Some(env.slot_view),
    };

    let Ok(rapira) = Rapira::start_worker(mode, hooks) else {
        return 1;
    };
    let Ok(handle) = rapira.handle() else {
        return 1;
    };

    extension_host::spawn_lifeline_watch(env.lifeline);

    let running = host.run(handle, script);
    let _ = stopper.set(running.stopper());
    // Quota/unhealthy may have fired before the stopper existed.
    if WORKER_EXIT.load(SeqCst) != -1 {
        stopper.get().expect("just set").stop();
    }

    let outcomes = running.serve_worker();
    drop(rapira); // joins the PHP thread; module teardown stays with the master

    // A decided protocol code (recycle/unhealthy) wins: those carry supervision
    // meaning (no-backoff respawn, gen-0 failboot) a drain error must not erase.
    // Only an error with no protocol code is reported as a crash.
    match WORKER_EXIT.load(SeqCst) {
        -1 if outcomes.iter().any(|o| o.is_err()) => 1,
        -1 => 0, // drained (QUIT/INT or natural finish)
        code => code,
    }
}
