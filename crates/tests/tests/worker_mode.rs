//! Worker mode (`\Rapira\handle_request`) smoke set - per-job superglobals,
//! the draining-false contract, the wrong-mode gates, one test per
//! cycle-terminal state - plus the per-job hygiene pins ($_ENV survival,
//! proto_num). The bulk of worker-mode coverage lives in the ported suites.

use php_sys::{Mode, Rapira};
use tests::{captured, drain, drain_resp, fixture, init_log_capture, php_lock, req};

/// Superglobals are rebuilt per job over the resident loop; the response is
/// the buffered classic-parity trio.
#[test]
fn worker_serves_with_per_job_superglobals() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("worker/hello-worker.php")))?;
    let h = r.handle();

    let resp = drain_resp(h.handle_blocking(req("/?q=zap", "worker/hello-worker.php"))?);
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_string(), "hello:GET:zap");
    // php appends default_charset to a bare text/* content-type
    assert_eq!(
        resp.header("content-type").as_deref(),
        Some("text/plain;charset=UTF-8"),
        "header() must reach the head"
    );

    let resp = drain_resp(h.handle_blocking(req("/", "worker/hello-worker.php"))?);
    assert_eq!(
        resp.body_string(),
        "hello:GET:-",
        "query state must not leak"
    );

    drop(h);
    r.shutdown();
    Ok(())
}

/// Closing the intake makes handle_request() return false; the script runs to
/// completion after the loop and the worker stops cleanly (Cycle::Stop).
#[test]
fn drain_returns_false_and_the_script_completes() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture("worker/drain-worker.php")))?;
    let h = r.handle();
    for want in ["n=1", "n=2"] {
        let resp = drain_resp(h.handle_blocking(req("/", "worker/drain-worker.php"))?);
        assert_eq!(resp.body_string(), want, "resident state must accumulate");
    }
    drop(h);
    r.shutdown(); // joins the worker: false reached the loop, the script wound down

    let exited = captured()
        .iter()
        .filter(|c| c.target == "app" && c.message == "loop-exited served=2")
        .count();
    assert_eq!(exited, 1, "the post-loop code must run exactly once");
    Ok(())
}

/// classic mode: the gate throws `NotInWorkerModeError` before any intake is
/// touched, and ZPP rejects a non-callable ahead of the gate.
#[test]
fn handle_request_outside_worker_mode_throws() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let (status, body) = drain(h.handle_blocking(req("/", "worker/gate-classic.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200, "every throw must be caught (body: {body:?})");
    for line in [
        "class: Rapira\\Exception\\NotInWorkerModeError",
        "rapira: yes",
        "type-error",
        "done",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    Ok(())
}

/// dispatcher mode: the gate refuses before the shared intake is touched - an
/// ungated call would steal the unit and serve it with no context bound.
#[test]
fn handle_request_in_dispatcher_mode_throws() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Dispatcher(fixture(
        "worker/gate-dispatcher-worker.php",
    )))?;
    let h = r.handle();
    let resp = drain_resp(h.handle_blocking(req("/", "worker/gate-dispatcher-worker.php"))?);
    assert_eq!(
        resp.body_string(),
        "ok",
        "the unit must survive the refusal"
    );
    drop(h);
    r.shutdown();

    let gated = captured()
        .iter()
        .filter(|c| {
            c.target == "app" && c.message == "gate Rapira\\Exception\\NotInWorkerModeError"
        })
        .count();
    assert_eq!(gated, 1);
    let finish_gated = captured()
        .iter()
        .filter(|c| c.target == "app" && c.message == "finish-gate")
        .count();
    assert_eq!(
        finish_gated, 1,
        "rapira_finish_request() must refuse dispatcher mode"
    );
    Ok(())
}

/// exit() inside a handler finishes that response and keeps the resident loop
/// alive with its state: EXIT is not a recycle.
#[test]
fn exit_in_a_handler_survives_the_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("worker/exit-worker.php")))?;
    let h = r.handle();

    let resp = drain_resp(h.handle_blocking(req("/?die=1", "worker/exit-worker.php"))?);
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_string(), "n=1", "exit must still ship the body");
    let resp = drain_resp(h.handle_blocking(req("/", "worker/exit-worker.php"))?);
    assert_eq!(resp.body_string(), "n=2", "the loop and its state survive");

    drop(h);
    r.shutdown();
    Ok(())
}

/// A script that ends its own loop with the channel open classifies Recycle:
/// the next job re-bootstraps and is served by the fresh cycle.
#[test]
fn self_stopping_loop_recycles_and_serves_again() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture("worker/one-turn-worker.php")))?;
    let h = r.handle();
    for _ in 0..2 {
        let resp = drain_resp(h.handle_blocking(req("/", "worker/one-turn-worker.php"))?);
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.body_string(), "once");
    }
    drop(h);
    r.shutdown();

    // two served cycles + the final drained bootstrap that saw closure
    let turns = captured()
        .iter()
        .filter(|c| c.target == "app" && c.message == "one-turn-done")
        .count();
    assert_eq!(turns, 3, "each bootstrap must run the script to completion");
    Ok(())
}

/// A bootstrap that never calls handle_request() is a boot failure: the shed
/// path answers 503, never a hang or an empty 200.
#[test]
fn never_looping_script_sheds_503() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("worker/never-loop-worker.php")))?;
    let h = r.handle();
    // deadline-bounded: the no-hang claim must fail as a test, not block the suite
    let mut rx = h.handle_blocking(req("/", "worker/never-loop-worker.php"))?;
    let resp = tests::drain_resp_deadline(
        &mut rx,
        std::time::Instant::now() + std::time::Duration::from_secs(10),
    )
    .expect("the shed 503 never arrived");
    assert_eq!(resp.status(), 503, "a never-serving bootstrap must shed");
    drop(h);
    r.shutdown();
    Ok(())
}

/// Bootstrap-populated $_ENV entries survive a later-compiled file that
/// mentions $_ENV: php_auto_globals_create_env dtors the array before checking
/// variables_order, so a re-armed _ENV would wipe them mid-cycle.
#[test]
fn bootstrap_env_survives_late_compilation() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("worker/env-worker.php")))?;
    let h = r.handle();
    for job in 0..2 {
        let resp = drain_resp(h.handle_blocking(req("/", "worker/env-worker.php"))?);
        assert_eq!(resp.body_string(), "set-at-boot", "job {job}");
    }
    drop(h);
    r.shutdown();
    Ok(())
}

/// `Location:` on a POST answers 303 on HTTP/1.1 (sapi_header_op's proto_num
/// arm). sapi_activate resets proto_num to 1000, so without the post-activate
/// re-apply every redirect degrades to 302.
#[test]
fn post_location_redirects_303_in_worker_mode() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("worker/location-worker.php")))?;
    let h = r.handle();
    let mut rq = req("/", "worker/location-worker.php");
    rq.method = "POST".into();
    let resp = drain_resp(h.handle_blocking(rq)?);
    assert_eq!(resp.status(), 303);
    assert_eq!(resp.header("location").as_deref(), Some("/elsewhere"));

    // GET keeps 302 - the arm is method-conditional
    let resp = drain_resp(h.handle_blocking(req("/", "worker/location-worker.php"))?);
    assert_eq!(resp.status(), 302);
    drop(h);
    r.shutdown();
    Ok(())
}

/// Classic mode has the same populate-before-activate defect; the shared
/// re-apply covers it too.
#[test]
fn post_location_redirects_303_in_classic_mode() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle();
    let mut rq = req("/", "worker/location-classic.php");
    rq.method = "POST".into();
    let resp = drain_resp(h.handle_blocking(rq)?);
    assert_eq!(resp.status(), 303);
    drop(h);
    r.shutdown();
    Ok(())
}

/// A client that vanishes while its job is still queued is discarded by the
/// pre-handout probe: without it the handler runs, its write aborts, and the
/// whole worker recycles for a request nobody is waiting on.
#[test]
fn queued_client_gone_is_discarded_before_handout() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();
    let r = Rapira::start(Mode::Worker(fixture("worker/held-worker.php")))?;
    let h = r.handle();

    // occupy the worker, then queue a job and abandon it while it waits
    let rx_a = h.handle_blocking(req("/", "worker/held-worker.php"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !captured()
        .iter()
        .any(|c| c.target == "app" && c.message == "held")
    {
        assert!(std::time::Instant::now() < deadline, "fixture never held");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    drop(h.handle_blocking(req("/", "worker/held-worker.php"))?); // abandoned while queued
    let resp_a = drain_resp(rx_a);
    assert_eq!(resp_a.body_string(), "done");

    let resp = drain_resp(h.handle_blocking(req("/?probe=count", "worker/held-worker.php"))?);
    // probe-discarded: 1 run, not 2 (ran + aborted) and not 0 (recycled)
    assert_eq!(resp.body_string(), "runs=1");
    drop(h);
    r.shutdown();
    Ok(())
}

/// handle_request() from inside its own handler is refused by the re-entrancy
/// guard; the outer job still completes.
#[test]
fn nested_handle_request_is_refused() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("worker/nested-worker.php")))?;
    let h = r.handle();
    // deadline-bounded: a guard regression that deadlocks must fail, not hang
    let mut rx = h.handle_blocking(req("/", "worker/nested-worker.php"))?;
    let resp = tests::drain_resp_deadline(
        &mut rx,
        std::time::Instant::now() + std::time::Duration::from_secs(10),
    )
    .expect("the outer response never arrived");
    assert_eq!(resp.status(), 200);
    assert!(
        resp.body_string()
            .contains("nested: handle_request() may not be called from inside its handler"),
        "got {:?}",
        resp.body_string()
    );
    drop(h);
    r.shutdown();
    Ok(())
}
