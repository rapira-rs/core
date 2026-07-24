use log::error;

use crate::{callbacks::*, scoreboard::sb_update, start::pull_job, types::Outcome};
use std::{
    cell::RefCell,
    os::raw::c_int,
    path::{Path, PathBuf},
};

use crate::{
    callbacks::guard,
    context::{bind_server_context, ctx, populate_request_context, unbind_server_context},
    executor::run_script,
    php_request_startup, rapira_pg, rapira_run_handler,
    types::Job,
    zend_fcall_info, zend_fcall_info_cache, *,
};

thread_local! {
    static WORKER: RefCell<Option<WorkerChan>> = const { RefCell::new(None) };
}

const UNHEALTHY_AFTER: u32 = 5;

enum Cycle {
    Stop,    // intake channel closed (Rapira dropped) - the only way a worker exits
    Recycle, // a job bailed - re-bootstrap immediately
    Failed,  // startup or bootstrap fatal - 503 one queued job, then retry the boot
    Restart, // php_request_shutdown bailed - rebuild the PHP thread state
}

pub enum WorkerExit {
    Closed,  // intake channel closed - worker_main exits the thread
    Restart, // worker_main drops PhpThread and builds a fresh one
}

// What the C `HttpHandler::handleRequest` does after a worker-loop turn. Keep in
// sync with handle_config.c.
#[repr(i32)]
enum HandleAction {
    Stop = 0,     // clean loop exit (intake channel closed) -> RETURN_BOOL(false)
    Continue = 1, // job served, keep looping -> RETURN_BOOL(true)
    Recycle = 2,  // a bailout occurred -> C raises zend_bailout to unwind the resident script
}

struct WorkerChan {
    first_call: bool,
    recycle: bool,
}

fn worker_recycle() -> bool {
    WORKER.with_borrow(|w| w.as_ref().is_some_and(|wc| wc.recycle))
}

/// Whether the resident worker loop owns this thread. `WORKER` is installed by
/// `rapira_worker` only, so this is false under `classic_worker`.
pub(crate) fn worker_mode_active() -> bool {
    WORKER.with_borrow(|w| w.is_some())
}

fn set_worker_recycle() {
    WORKER.with_borrow_mut(|w| {
        if let Some(wc) = w.as_mut() {
            wc.recycle = true;
        }
    });
}

fn run_cycle(script: &Path) -> Cycle {
    let started = unsafe { php_request_startup() } == SUCCESS;
    if !started {
        error!(target: "rapira", "php_request_startup() failed");
    }
    let completed = started && unsafe { run_script(script) };

    let recycle = WORKER.with_borrow_mut(|w| {
        w.as_mut().is_some_and(|wc| {
            wc.first_call = true; // next cycle re-runs the bootstrap
            std::mem::take(&mut wc.recycle)
        })
    });

    // php_request_shutdown frees PG(last_error_message) (main.c:2024) —
    // log the bootstrap fatal before it disappears
    log_and_clear_last_error();
    if Outcome::from_c(unsafe { rapira_request_shutdown() }) == Outcome::Bailout {
        // the retry reclaimed the request, but the bailed observer walk skipped
        // end handlers — per-thread extension state is suspect, rebuild it
        error!(target: "rapira", "php_request_shutdown() bailed; restarting the PHP thread");
        sb_update(scoreboard::Event::Restart);
        return Cycle::Restart;
    }

    if completed && !recycle {
        Cycle::Stop
    } else if recycle {
        Cycle::Recycle
    } else {
        Cycle::Failed
    }
}

pub fn rapira_worker(script: PathBuf) -> WorkerExit {
    WORKER.with_borrow_mut(|w| {
        *w = Some(WorkerChan {
            first_call: true,
            recycle: false,
        })
    });

    let mut failures: u32 = 0;
    let exit = loop {
        match run_cycle(&script) {
            Cycle::Stop => break WorkerExit::Closed,
            Cycle::Restart => break WorkerExit::Restart,
            Cycle::Recycle => failures = 0,
            Cycle::Failed => {
                failures += 1;
                if failures == UNHEALTHY_AFTER {
                    error!(target: "rapira", "worker keeps failing to boot; flagged unhealthy");
                    sb_update(scoreboard::Event::Unhealthy);
                }
                // Can't run PHP. Answer one queued job with 503, then loop to
                // retry the boot (demand-driven — no jobs means we block cheaply
                // here). None == Rapira dropped: exit instead of hanging Drop.
                match pull_job() {
                    None => break WorkerExit::Closed,
                    Some(mut job) => {
                        send_error_head(&mut job.ctx, 503);
                        job.ctx.finish(false);
                        sb_update(scoreboard::Event::Shed);
                    }
                }
            }
        }
    };
    log_and_clear_last_error();
    exit
}

/// # Safety
/// Invoked from C (`HttpHandler::handleRequest`) once per worker-loop
/// iteration. `fci` and `fcc` must be valid, non-null pointers produced by
/// `Z_PARAM_FUNC` and remain valid for the call. Must run on the resident worker
/// thread whose `WORKER` thread-local is initialized, inside its active request.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_handle_request(
    fci: *mut zend_fcall_info,
    fcc: *mut zend_fcall_info_cache,
) -> c_int {
    // A caught Rust panic recycles (rebuild over a suspect thread), never silently continues.
    let action = guard(HandleAction::Recycle, || handle_request_impl(fci, fcc));
    // A caught panic in handle_request_impl skips its unbind_server_context, leaving
    // SG(server_context) dangling to the freed job; run_cycle's php_request_shutdown
    // would then flush output through it. Clear it here (idempotent on the normal
    // path, which already unbound).
    unbind_server_context();
    action as c_int
}

fn handle_request_impl(fci: *mut zend_fcall_info, fcc: *mut zend_fcall_info_cache) -> HandleAction {
    let Some(mut job) = next_job() else {
        // None is terminal: next_job set wc.recycle on a first-call teardown bailout (unwind the
        // script), else the intake channel closed (clean stop).
        return if worker_recycle() {
            HandleAction::Recycle
        } else {
            HandleAction::Stop
        };
    };

    bind_server_context(&mut job.ctx);
    unsafe {
        populate_request_context(&mut job.ctx);
        rapira_release_temporary_streams();
    }

    let mut outcome = Outcome::from_c(unsafe { rapira_request_activate() });
    if outcome != Outcome::Bailout {
        outcome = Outcome::from_c(unsafe { rapira_run_handler(fci, fcc) });
    }

    // The handler has returned: from here every ub_write is a teardown flush, not
    // streaming, so mark the context tearing down before flushing. This freezes
    // `stream` — a buffered body pushed out now stays a complete response; only body
    // streamed *during* the handler counts as truncated (see `Context::is_truncated`).
    job.ctx.tearing_down = true;
    // the real head (status, cookies, php_error_cb's 500) lives in
    // SG(sapi_headers); teardown destroys it — flush first
    let flushed = match outcome {
        Outcome::Bailout | Outcome::Throw => Outcome::from_c(unsafe { rapira_finish_output() }),
        _ => Outcome::Ok,
    };
    let teardown: Outcome = Outcome::from_c(unsafe { rapira_request_teardown() });

    // every contained bailout recycles: only php_request_shutdown may observe the
    // Zend state a longjmp leaves behind (a live VM stack, a mark-destructed
    // object store, an emalloc arena the executor never unwound)
    // https://man7.org/linux/man-pages/man3/setjmp.3.html
    let recycle: bool = [outcome, flushed, teardown].contains(&Outcome::Bailout);
    // an uncaught throw is an error response but doesn't need a recycle
    let errored: bool = recycle || outcome == Outcome::Throw;
    let truncated: bool = finalize_response(&mut job.ctx, errored);

    log_and_clear_last_error();
    unbind_server_context();
    sb_update(scoreboard::Event::Handled(errored));
    if recycle {
        sb_update(scoreboard::Event::Recycled);
        set_worker_recycle();
    }
    job.ctx.finish(truncated);
    // Recycle tells C to zend_bailout so no PHP runs over the post-longjmp state; the response was
    // already sealed by job.ctx.finish() above. Continue keeps the resident loop going.
    if recycle {
        HandleAction::Recycle
    } else {
        HandleAction::Continue
    }
}

// worker-mode wrapper, still called from inside the PHP loop (via HttpHandler::handleRequest):
fn next_job() -> Option<Job> {
    WORKER.with_borrow_mut(|w| {
        let wc = w.as_mut()?;
        // first iteration: clean up whatever php_request_startup()'s bootstrap
        // left before serving real requests — there's no prior request yet
        if std::mem::take(&mut wc.first_call) {
            let outcome = Outcome::from_c(unsafe { rapira_request_teardown() });
            if outcome == Outcome::Bailout {
                // only php_request_shutdown reclaims the state a longjmp left behind
                // (php-src main.c) - recycle instead of serving on top of it
                error!(target: "rapira", "rapira_request_teardown() bailed on first call; recycling");
                wc.recycle = true;
                return None;
            }
            sb_update(scoreboard::Event::Healthy);
        }
        log_and_clear_last_error();
        pull_job()
    })
}

/// # Safety
/// Called from C (`rapira_finish_request`). Must run on a worker thread inside an
/// active request whose `Context` is bound in `SG(server_context)`.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_finish_response() {
    guard((), || unsafe {
        if let Some(c) = ctx() {
            c.finish(false);
        }
    });
}

fn log_and_clear_last_error() {
    unsafe {
        let zend_str = (*rapira_pg()).last_error_message;
        if !zend_str.is_null() {
            let msg =
                std::slice::from_raw_parts((*zend_str).val.as_ptr().cast::<u8>(), (*zend_str).len);
            error!(target: "php", "last error: {}", String::from_utf8_lossy(msg));
        }
        rapira_clear_last_error();
    }
}
