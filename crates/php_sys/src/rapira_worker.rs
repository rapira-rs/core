use log::error;

use crate::{callbacks::*, scoreboard::sb_update, start::pull_job, types::Outcome};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
};

use crate::{
    callbacks::guard,
    context::{bind_server_context, ctx, populate_request_context, unbind_server_context},
    executor::run_script,
    php_request_startup, rapira_pg, rapira_run_handler,
    start::JobRx,
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

struct WorkerChan {
    rx: JobRx,
    first_call: bool,
    recycle: bool,
}

fn run_cycle(script: &Path) -> Cycle {
    let started = unsafe { php_request_startup() } == SUCCESS;
    if !started {
        error!("[rapira] php_request_startup() failed");
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
    if matches!(
        unsafe { rapira_request_shutdown() },
        types::Outcome::Bailout
    ) {
        // the retry reclaimed the request, but the bailed observer walk skipped
        // end handlers — per-thread extension state is suspect, rebuild it
        error!("[rapira] php_request_shutdown() bailed; restarting the PHP thread");
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

pub fn rapira_worker(script: PathBuf, rx: JobRx) -> WorkerExit {
    WORKER.with_borrow_mut(|w| {
        *w = Some(WorkerChan {
            rx: rx.clone(),
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
                    error!("[rapira] worker keeps failing to boot; flagged unhealthy");
                    sb_update(scoreboard::Event::Unhealthy);
                }
                // Can't run PHP. Answer one queued job with 503, then loop to
                // retry the boot (demand-driven — no jobs means we block cheaply
                // here). None == Rapira dropped: exit instead of hanging Drop.
                match pull_job(&rx) {
                    None => break WorkerExit::Closed,
                    Some(mut job) => {
                        send_error_head(&mut job.ctx, 503);
                        job.ctx.finish();
                        sb_update(scoreboard::Event::Handled(true));
                    }
                }
            }
        }
    };
    log_and_clear_last_error();
    exit
}

/// # Safety
/// Invoked from C (the `rapira_handle_request` PHP function) once per worker-loop
/// iteration. `fci` and `fcc` must be valid, non-null pointers produced by
/// `Z_PARAM_FUNC` and remain valid for the call. Must run on the resident worker
/// thread whose `WORKER` thread-local is initialized, inside its active request.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_handle_request(
    // Safety: not safe
    fci: *mut zend_fcall_info,
    fcc: *mut zend_fcall_info_cache,
) -> bool {
    let ok = guard(false, || handle_request_impl(fci, fcc));
    // A caught panic in handle_request_impl skips its unbind_server_context, leaving
    // SG(server_context) dangling to the freed job; run_cycle's php_request_shutdown
    // would then flush output through it. Clear it here (idempotent on the normal
    // path, which already unbound).
    unbind_server_context();
    ok
}

fn handle_request_impl(fci: *mut zend_fcall_info, fcc: *mut zend_fcall_info_cache) -> bool {
    let Some(mut job) = next_job() else {
        return false;
    };

    bind_server_context(&mut job.ctx);
    unsafe {
        populate_request_context(&mut job.ctx);
        rapira_release_temporary_streams();
    }

    let mut outcome = unsafe { rapira_request_activate() };
    if outcome != Outcome::Bailout {
        outcome = unsafe { rapira_run_handler(fci, fcc) };
    }

    // the real head (status, cookies, php_error_cb's 500) lives in
    // SG(sapi_headers); teardown destroys it — flush first
    let flushed = match outcome {
        Outcome::Bailout | Outcome::Throw => unsafe { rapira_finish_output() },
        _ => Outcome::Ok,
    };
    let teardown: Outcome = unsafe { rapira_request_teardown() };

    // every contained bailout recycles: only php_request_shutdown may observe the
    // Zend state a longjmp leaves behind (a live VM stack, a mark-destructed
    // object store, an emalloc arena the executor never unwound)
    let recycle: bool = [outcome, flushed, teardown].contains(&Outcome::Bailout);
    // an uncaught throw is an error response but doesn't need a recycle
    let errored: bool = recycle || outcome == Outcome::Throw;

    if errored && !job.ctx.headers_sent {
        send_error_head(&mut job.ctx, 500);
    }

    log_and_clear_last_error();
    unbind_server_context();
    sb_update(scoreboard::Event::Handled(errored));
    if recycle {
        sb_update(scoreboard::Event::Recycled);
        WORKER.with_borrow_mut(|w| {
            if let Some(wc) = w.as_mut() {
                wc.recycle = true;
            }
        });
    }
    job.ctx.finish();
    // false breaks the PHP worker loop so run_cycle can rebuild the request
    !recycle
}

// worker-mode wrapper, still called from inside the PHP loop (via rapira_handle_request):
fn next_job() -> Option<Job> {
    WORKER.with_borrow_mut(|w| {
        let wc = w.as_mut()?;
        // first iteration: clean up whatever php_request_startup()'s bootstrap
        // left before serving real requests — there's no prior request yet
        if std::mem::take(&mut wc.first_call) {
            let outcome: types::Outcome = unsafe { rapira_request_teardown() };
            if matches!(outcome, types::Outcome::Bailout) {
                // only php_request_shutdown reclaims the state a longjmp left behind
                // (php-src main.c) - recycle instead of serving on top of it
                error!("[rapira] rapira_request_teardown() bailed on first call; recycling");
                wc.recycle = true;
                return None;
            }
            sb_update(scoreboard::Event::Healthy);
        }
        log_and_clear_last_error();
        pull_job(&wc.rx)
    })
}

/// # Safety
/// Called from C (`rapira_finish_request`). Must run on a worker thread inside an
/// active request whose `Context` is bound in `SG(server_context)`.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_finish_response() {
    guard((), || unsafe {
        if let Some(c) = ctx() {
            c.finish();
        }
    });
}

fn log_and_clear_last_error() {
    unsafe {
        let zend_str = (*rapira_pg()).last_error_message;
        if !zend_str.is_null() {
            let msg =
                std::slice::from_raw_parts((*zend_str).val.as_ptr().cast::<u8>(), (*zend_str).len);
            error!("[rapira] last PHP error: {}", String::from_utf8_lossy(msg));
        }
        // null out the last error message
        rapira_clear_last_error();
    }
}
