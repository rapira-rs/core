//! max_execution_time enforcement in worker mode. Own test binary: it overrides
//! PHPRC (timeout.php.ini, max_execution_time=1), which is process-global.
//!
//! The per-job re-arm (`rapira_request_init` → `zend_set_timeout`) passes
//! reset_signals=0 — the SIGRTMIN handler is installed once per cycle by
//! php_request_startup, not per job. These tests prove the timer armed that way
//! still fires on jobs after the first in a cycle, and that the worker recovers
//! afterwards.
//!
//! Skipped on macOS/Windows: rapira arms a per-request timeout only where Zend's per-thread timer
//! exists (`ZEND_MAX_EXECUTION_TIMERS`, Linux/FreeBSD-only — needs POSIX timer_create, which Darwin
//! and Windows lack), so elsewhere the busy-loop fixture would spin forever.
//! https://github.com/php/php-src/pull/10141
//! https://man7.org/linux/man-pages/man2/timer_create.2.html
//! https://man7.org/linux/man-pages/man7/signal.7.html
#![cfg(not(any(target_os = "macos", target_os = "windows")))]

use php_sys::{Mode, Rapira};
use std::path::Path;
use tests::{drain, fixture, php_lock_with_ini, req};

/// The receive() timer discipline: the wall timer is disarmed while the worker
/// parks in receive() and re-armed with the captured budget per unit, so time
/// spent waiting for work never counts against max_execution_time. With the
/// discipline broken, the 1s budget fires mid-park, the cycle fatals, and the
/// parked job is 503-shed instead of served.
#[test]
fn parked_receive_outlives_the_execution_budget() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/timeout_tests/timeout.php.ini"
    )));
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/echo-loop-worker.php")))?;
    let h = r.handle()?;

    // Park the worker well past the 1s budget, twice: the first wait proves
    // the boot-time capture disarmed the timer, the second that the per-unit
    // re-arm was disarmed again by the next receive().
    for target in ["/first", "/second"] {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let (status, body) =
            drain(h.handle_blocking(req(target, "dispatcher/echo-loop-worker.php"))?);
        assert_eq!(
            (status, body.as_str()),
            (200, "method=GET body="),
            "a worker parked past the budget must still serve {target}"
        );
    }

    drop(h);
    r.shutdown();
    Ok(())
}

/// The other half of the discipline: the budget re-armed at unit handout must
/// still fire. A unit that spins forever is killed by max_execution_time, the
/// unit fails upstream (its frame dies unsent), and the recycled worker keeps
/// serving.
#[test]
fn rearmed_budget_kills_a_spinning_unit() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/timeout_tests/timeout.php.ini"
    )));
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/verbs-worker.php")))?;
    let h = r.handle()?;

    // Serve one unit first so the timer fatal lands as Recycle (served > 0),
    // not as a boot failure that sheds the next request.
    let (status, body) = drain(h.handle_blocking(req("/", "dispatcher/verbs-worker.php"))?);
    assert_eq!((status, body.as_str()), (200, "state=false"));

    // Bounded wait: a silent re-arm regression must fail the test, not hang
    // the suite.
    let mut rx = h.handle_blocking(req("/?probe=spin", "dispatcher/verbs-worker.php"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match rx.try_recv() {
            Ok(frame) => panic!(
                "a spinning unit must not seal a frame (got status {:?})",
                frame.head.map(|h| h.status)
            ),
            // the unit died unsealed: the timer fired and the cycle recycled
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "spinning unit was never killed — the per-unit budget did not re-arm"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    let (status, body) = drain(h.handle_blocking(req("/", "dispatcher/verbs-worker.php"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "state=false"),
        "the worker must recover after the timeout"
    );

    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn max_execution_time_fires_on_rearmed_jobs() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/timeout_tests/timeout.php.ini"
    )));
    let r = Rapira::start(Mode::Dispatcher(fixture(
        "timeout_tests/timeout-worker.php",
    )))?;
    let h = r.handle()?;

    // Job 1: a fast request is untouched by the 1s cap.
    let (status, body) = drain(h.handle_blocking(req(
        "/timeout-worker.php",
        "timeout_tests/timeout-worker.php",
    ))?);
    assert_eq!((status, body.as_str()), (200, "ok"));

    // Job 2, same cycle (job 1 did not recycle): the timer re-armed by
    // rapira_request_init with reset_signals=0 must still deliver SIGRTMIN and
    // kill the spin — the regression surface of not reinstalling the handler.
    // Bounded wait: a silent signal-delivery regression must fail the test, not
    // hang the suite.
    let mut rx = h.handle_blocking(req(
        "/timeout-worker.php?mode=spin",
        "timeout_tests/timeout-worker.php",
    ))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let frame = loop {
        match rx.try_recv() {
            Ok(frame) => break frame,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "spinning job was never killed — max_execution_time did not fire"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                panic!("worker died without sealing a response");
            }
        }
    };
    let body = String::from_utf8_lossy(&frame.body).into_owned();
    assert!(
        body.contains("Maximum execution time"),
        "the timeout fatal must reach the body (got: {body:?})"
    );
    // The fatal fires mid-display: the error output itself committed the head as
    // 200 before php_error_cb could swap in a 500 (it only does so while no
    // headers are sent) — canonical PHP behavior for a mid-output fatal with
    // display_errors=On.
    assert_eq!(frame.head.map(|h| h.status), Some(200));

    // The worker recovers: the timeout bailed out and recycled the cycle, and the
    // next job is served normally. (The teardown disarm itself has no PHP-visible
    // effect to assert on this build: the per-job re-arm clears EG(timed_out), so
    // even a stale idle-fired timeout would be consumed harmlessly.)
    let (status, body) = drain(h.handle_blocking(req(
        "/timeout-worker.php",
        "timeout_tests/timeout-worker.php",
    ))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "ok"),
        "the worker must recover after a timeout"
    );

    drop(h);
    r.shutdown();
    Ok(())
}
