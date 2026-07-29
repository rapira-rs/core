use std::thread;

use php_sys::{Mode, Rapira};
use tests::{captured, drain, fixture, init_log_capture, php_lock, req};

// this test works on both zts and nts
#[test]
fn hello_world_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?; // single interpreter => same one for both reqs
    let h = r.handle()?;
    let (_, body1) = drain(h.handle_blocking(req("/?x=1", "hello.php"))?);
    assert!(
        body1.contains("Hello, anonymous!") && body1.contains("Method: GET"),
        "req1 baseline (got: {body1:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn fibers_stress_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(req("/", "fibers.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(
        status, 200,
        "fiber script must compile + run without a stack-guard fatal (got {status}, body {body:?})"
    );
    assert!(
        body.contains("fibers ok sum=226644"),
        "fibers must complete with the correct total (got: {body:?})"
    );
    Ok(())
}

#[test]
fn hello_world_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("worker.php")))?; // single interpreter => same one for both reqs
    let h = r.handle()?;
    let (_, body1) = drain(h.handle_blocking(req("/?x=1", "worker.php"))?);
    assert!(
        body1.contains("Hello from worker, anonymous!"),
        "req1 baseline (got: {body1:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn worker_request_isolation() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("leak-worker.php")))?; // single interpreter => same one for both reqs
    let h = r.handle()?;
    let (_, body1) = drain(h.handle_blocking(req("/?x=1", "leak-worker.php"))?);
    let (_, body2) = drain(h.handle_blocking(req("/?x=2", "leak-worker.php"))?);
    assert!(
        body1.contains("counter=1") && body1.contains("session=clean"),
        "req1 baseline (got: {body1:?})"
    );
    assert!(
        body2.contains("session=clean"),
        "$_SESSION must reset between requests (got: {body2:?})"
    );
    assert!(
        body2.contains("counter=2"),
        "static class props persist across requests by design (got: {body2:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn worker_survives_exit() -> anyhow::Result<()> {
    let _guard = php_lock();

    let r = Rapira::start(Mode::Worker(fixture("bailout-worker.php")))?;
    let h = r.handle()?;
    let (s1, b1) = drain(h.handle_blocking(req("/?boom=0", "bailout-worker.php"))?); // normal
    let (s2, b2) = drain(h.handle_blocking(req("/?boom=1", "bailout-worker.php"))?); // exit(1) -> unwind-exit
    let (s3, b3) = drain(h.handle_blocking(req("/?boom=0", "bailout-worker.php"))?); // worker must still serve

    assert_eq!(s1, 200);
    assert!(b1.contains("ok counter=1"), "req1 (got: {b1:?})");

    // exit() before any output: graceful unwind, empty body, default 200
    assert_eq!(
        s2, 200,
        "exit() is a graceful unwind, not a 500 (got status {s2}, body {b2:?})"
    );
    assert!(
        b2.is_empty(),
        "exit(1) before any output => empty body (got: {b2:?})"
    );

    // worker survived exit(), serves the next request
    assert_eq!(s3, 200, "worker must recover after exit() (got {s3})");
    assert!(
        b3.contains("ok counter=3"),
        "worker must survive exit() and serve the next request (got: {b3:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn fibers_stress_worker() -> anyhow::Result<()> {
    let _guard = php_lock();

    let r = Rapira::start(Mode::Worker(fixture("fibers-worker.php")))?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(req("/", "fibers-worker.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(
        status, 200,
        "fiber script must compile + run without a stack-guard fatal (got {status}, body {body:?})"
    );
    assert!(
        body.contains("fibers ok sum=226644"),
        "fibers must complete with the correct total (got: {body:?})"
    );
    Ok(())
}

#[test]
fn worker_survives_teardown_bailout() -> anyhow::Result<()> {
    let _guard = php_lock();

    let r = Rapira::start(Mode::Worker(fixture("teardown-bailout-worker.php")))?;
    let h = r.handle()?;
    let (s1, b1) = drain(h.handle_blocking(req("/?boom=0", "teardown-bailout-worker.php"))?); // normal
    let (s2, b2) = drain(h.handle_blocking(req("/?boom=1", "teardown-bailout-worker.php"))?); // bail in teardown
    let (s3, b3) = drain(h.handle_blocking(req("/?boom=0", "teardown-bailout-worker.php"))?); // must still serve

    assert_eq!(s1, 200);
    assert!(b1.contains("ok counter=1"), "req1 baseline (got: {b1:?})");

    // teardown bailout commits a 500 head; buffered body lost
    assert_eq!(
        s2, 500,
        "teardown-flush bailout commits a 500 head (got {s2}, body {b2:?})"
    );
    assert!(
        b2.is_empty(),
        "buffered body is lost to the bailout (got: {b2:?})"
    );

    // the teardown bailout recycles the worker: full php_request_shutdown +
    // re-run bootstrap — statics reset, so the counter starts over
    assert_eq!(
        s3, 200,
        "worker must recover after a teardown bailout (got {s3})"
    );
    assert!(
        b3.contains("ok counter=1"),
        "recycle re-runs the bootstrap; statics reset (got: {b3:?})"
    );

    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn many_producers_test() -> anyhow::Result<()> {
    let _guard = php_lock();

    let r = Rapira::start(Mode::Worker(fixture("worker.php")))?;

    let producers: Vec<_> = (0..24)
        .map(|t| {
            let h: php_sys::RapiraHandle = r.handle().unwrap();
            thread::spawn(move || {
                for i in 0..256 {
                    let name: String = format!("t{t}-r{i}");
                    let rx = h
                        .handle_blocking(req(&format!("/?name={name}"), "worker.php"))
                        .expect("ruuuun!");
                    let (status, body) = drain(rx);
                    assert_eq!(
                        status, 200,
                        "worker must serve (got {status}, body {body:?})"
                    );
                    assert!(
                        body.contains(&format!("Hello from worker, {name}!")),
                        "worker must serve (got: {body:?})"
                    );
                }
            })
        })
        .collect::<Vec<_>>();

    for p in producers {
        p.join().expect("thread join failed");
    }

    r.shutdown();
    Ok(())
}

#[test]
fn worker_basic_auth() -> anyhow::Result<()> {
    let _guard = php_lock();

    let r = Rapira::start(Mode::Worker(fixture("auth-worker.php")))?;
    let h = r.handle()?;

    // Authorization: Basic base64("user:pass")
    let mut with_auth = req("/", "auth-worker.php");
    with_auth
        .headers
        .push(("Authorization".into(), "Basic dXNlcjpwYXNz".into()));
    let (s_auth, b_auth) = drain(h.handle_blocking(with_auth)?);

    // no Authorization header on the next request
    let (s_none, b_none) = drain(h.handle_blocking(req("/", "auth-worker.php"))?);

    drop(h);
    r.shutdown();

    assert_eq!(s_auth, 200);
    assert!(
        b_auth.contains("user=user") && b_auth.contains("pass=pass"),
        "Basic auth must populate PHP_AUTH_USER/PW (got: {b_auth:?})",
    );

    // proves auth doesn't leak across requests
    assert_eq!(s_none, 200);
    assert!(
        b_none.contains("user=- pass=-"),
        "no auth header -> no PHP_AUTH vars (got: {b_none:?})",
    );

    Ok(())
}

#[test]
fn server_variables() -> anyhow::Result<()> {
    let _guard = php_lock();

    let r: Rapira = Rapira::start(Mode::Worker(fixture("server-variables.php")))?;
    let h: php_sys::RapiraHandle = r.handle()?;

    let mut request: php_sys::Request =
        req("/server-variables.php?foo=a&bar=b", "server-variables.php");
    request.method = "POST".into();
    request.content_type = Some("text/plain".into());
    request.content_length = 3;
    request.body = Box::new(std::io::Cursor::new(b"foo".to_vec()));
    request
        .headers
        .push(("Authorization".into(), "Basic dmFsZXJ5OnBhc3N3b3Jk".into()));

    let (status, body) = drain(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    for expected in [
        "[REQUEST_METHOD] => POST",
        "[QUERY_STRING] => foo=a&bar=b",
        "[REQUEST_URI] => /server-variables.php?foo=a&bar=b",
        "[CONTENT_TYPE] => text/plain",
        "[CONTENT_LENGTH] => 3",
        "[REMOTE_ADDR] => 127.0.0.1",
        "[SERVER_NAME] => localhost",
        "[SERVER_PORT] => 8080",
        "[SERVER_PROTOCOL] => HTTP/1.1",
        "[SERVER_SOFTWARE] => Rapira",
        "[HTTP_AUTHORIZATION] => Basic dmFsZXJ5OnBhc3N3b3Jk",
        "[PHP_AUTH_USER] => valery",
        "[PHP_AUTH_PW] => password",
    ] {
        assert!(
            body.contains(expected),
            "$_SERVER missing {expected:?} (got: {body:?})"
        );
    }
    Ok(())
}

#[test]
fn worker_finish_request() -> anyhow::Result<()> {
    let _guard = php_lock();

    let r = Rapira::start(Mode::Worker(fixture("finish-request-worker.php")))?;
    let h = r.handle()?;

    let (s1, b1) = drain(h.handle_blocking(req("/", "finish-request-worker.php"))?);
    let (s2, b2) = drain(h.handle_blocking(req("/", "finish-request-worker.php"))?);

    drop(h);
    r.shutdown();

    // output BEFORE rapira_finish_request() is flushed to the client
    assert_eq!(s1, 200);
    assert!(
        b1.contains("count=0") && b1.contains("BEFORE"),
        "pre-finish output must reach the client (got: {b1:?})"
    );
    // output AFTER it is dropped — finish() cleared the response sender (tx = None)
    assert!(
        !b1.contains("AFTER"),
        "post-finish output must NOT reach the client (got: {b1:?})"
    );

    // worker survived, and the post-response work (State::$n++) ran after the stream closed
    assert_eq!(s2, 200);
    assert!(
        b2.contains("count=1"),
        "work after rapira_finish_request() must still execute (got: {b2:?})"
    );
    assert!(
        !b2.contains("AFTER"),
        "post-finish output stays dropped on the next request too (got: {b2:?})"
    );

    Ok(())
}

#[test]
fn getenv_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    unsafe {
        std::env::set_var("FOO", "BAR");
    }
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(req("/", "env.php"))?);
    drop(h);
    r.shutdown();
    assert_eq!(status, 200);
    assert!(body.contains("BAR"), "expected FOO=BAR (got: {body:?})");
    Ok(())
}

#[test]
fn getenv_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    unsafe {
        std::env::set_var("FOO", "BAR");
    }
    let r = Rapira::start(Mode::Worker(fixture("env-worker.php")))?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(req("/", "env-worker.php"))?);
    drop(h);
    r.shutdown();
    assert_eq!(status, 200);
    assert!(body.contains("BAR"), "expected FOO=BAR (got: {body:?})");
    Ok(())
}

#[test]
fn failboot_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(req("/", "failboot.php"))?);
    drop(h);
    r.shutdown();
    assert_eq!(status, 200);
    assert!(
        body.contains("syntax error, unexpected end of file, expecting variable or"),
        "expected error trace (got: {body:?})"
    );
    Ok(())
}

#[test]
fn scoreboard_counts_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("throw-worker.php")))?;
    let h = r.handle()?;
    let _ = drain(h.handle_blocking(req("/?boom=0", "throw-worker.php"))?); // ok
    let _ = drain(h.handle_blocking(req("/?boom=0", "throw-worker.php"))?); // ok
    let _ = drain(h.handle_blocking(req("/?boom=1", "throw-worker.php"))?); // throw -> error
    drop(h);
    let snap = r.scoreboard();
    r.shutdown();

    assert_eq!(snap.handled, 3, "3 requests handled");
    assert_eq!(snap.errors, 1, "one engine error (uncaught throw)");
    assert_eq!(snap.workers.len(), 1);
    assert_eq!(snap.workers[0].handled, 3);
    assert_eq!(snap.workers[0].errors, 1);
    Ok(())
}

#[test]
fn scoreboard_counts_recycles_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("shutdown-fatal-worker.php")))?;
    let h = r.handle()?;
    // the shutdown-fn fatal bails in php_call_shutdown_functions -> recycle
    let _ = drain(h.handle_blocking(req("/?boom=1", "shutdown-fatal-worker.php"))?);
    let (s2, _) = drain(h.handle_blocking(req("/", "shutdown-fatal-worker.php"))?); // recovered
    drop(h);
    let snap = r.scoreboard();
    r.shutdown();

    assert_eq!(s2, 200, "worker recovers after the recycle");
    assert_eq!(snap.handled, 2, "both jobs handled");
    assert!(
        snap.recycles >= 1,
        "the shutdown-fn fatal must recycle the worker (recycles={})",
        snap.recycles
    );
    assert_eq!(snap.workers.len(), 1);
    assert!(
        snap.workers[0].recycles >= 1,
        "the recycle must be attributed to the worker"
    );
    Ok(())
}

// Multi-worker scoreboard aggregation returns as a multi-process test once the
// fork-based pool and the shared-memory scoreboard land (E2E harness batch).

#[test]
fn scoreboard_counts_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let _ = drain(h.handle_blocking(req("/", "hello.php"))?); // ok
    let _ = drain(h.handle_blocking(req("/", "hello.php"))?); // ok
    let _ = drain(h.handle_blocking(req("/", "failboot.php"))?); // parse error -> run_script() == false -> errored
    drop(h);
    let snap = r.scoreboard();
    r.shutdown();

    assert_eq!(snap.handled, 3, "3 requests handled");
    assert_eq!(
        snap.errors, 1,
        "one PHP error (failboot.php fails to compile)"
    );
    assert_eq!(snap.workers.len(), 1);
    assert_eq!(snap.workers[0].handled, 3);
    assert_eq!(snap.workers[0].errors, 1);
    Ok(())
}

#[test]
fn worker_session_isolation() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("session-worker.php")))?;
    let h = r.handle()?;
    let (s1, b1) = drain(h.handle_blocking(req("/", "session-worker.php"))?);
    let (s2, b2) = drain(h.handle_blocking(req("/", "session-worker.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(s1, 200);
    assert_eq!(s2, 200);
    assert!(b1.contains("n=0"), "req1 fresh session (got: {b1:?})");
    // without the fix: session_status stays active + $_SESSION leaks -> req2 sees n=1
    assert!(
        b2.contains("n=0"),
        "session must reset between worker requests (got: {b2:?})"
    );
    let sid = |b: &str| {
        b.split_whitespace()
            .find_map(|t| t.strip_prefix("sid=").map(str::to_owned))
    };
    assert_ne!(
        sid(&b1),
        sid(&b2),
        "each request must get a fresh session id (b1={b1:?}, b2={b2:?})"
    );
    Ok(())
}

#[test]
fn worker_bootstrap_output_is_logged() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear(); // drop anything captured by earlier tests

    let r = Rapira::start(Mode::Worker(fixture("boot-output-worker.php")))?;
    let h = r.handle()?;
    let (status, _) = drain(h.handle_blocking(req("/", "boot-output-worker.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200, "worker still serves after no-context output");
    let logged = captured();
    assert!(
        logged
            .iter()
            .any(|c| c.message.contains("WORKER-BOOT-OUTPUT")),
        "worker bootstrap output must be logged (captured: {logged:?})"
    );
    Ok(())
}

/// Levels of the worker's teardown last-error records carrying `mark`. Anchored on that line's
/// own prefix so each logging path is asserted separately, and so a `php`-target record that
/// only quotes the marker — a stack frame, say — is never counted as a diagnostic.
fn last_error_levels(logged: &[tests::Captured], mark: &str) -> Vec<log::Level> {
    php_levels(logged, mark, true)
}

/// Levels of the SAPI log-callback records carrying `mark` — the `php` target minus the
/// teardown line.
fn log_callback_levels(logged: &[tests::Captured], mark: &str) -> Vec<log::Level> {
    php_levels(logged, mark, false)
}

fn php_levels(logged: &[tests::Captured], mark: &str, teardown: bool) -> Vec<log::Level> {
    logged
        .iter()
        .filter(|c| {
            c.target == "php"
                && c.message.starts_with("last error: ") == teardown
                && c.message.contains(mark)
        })
        .map(|c| c.level)
        .collect()
}

#[test]
fn php_diagnostics_log_at_their_error_type_level() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture("error-levels-worker.php")))?;
    let h = r.handle()?;
    for step in ["deprecated", "warn", "boom"] {
        let uri = format!("/?step={step}");
        let _ = drain(h.handle_blocking(req(&uri, "error-levels-worker.php"))?);
    }
    drop(h);
    r.shutdown();

    // display_errors sends the text to the body and log_errors is off, so the teardown line is
    // the only php-target record per request -- the exact counts also catch double logging
    let logged = captured();
    assert_eq!(
        last_error_levels(&logged, "MASKED-DEPRECATION"),
        vec![log::Level::Trace],
        "a diagnostic the script masked must not reach error (captured: {logged:?})"
    );
    assert_eq!(
        last_error_levels(&logged, "REPORTED-WARNING"),
        vec![log::Level::Warn],
        "an unmasked E_USER_WARNING logs at warn (captured: {logged:?})"
    );
    assert_eq!(
        last_error_levels(&logged, "REPORTED-FATAL"),
        vec![log::Level::Error],
        "an uncaught throw stays an error-level diagnostic (captured: {logged:?})"
    );
    Ok(())
}

// error_reporting(0) masks every type php-src would mask, fatals included; the recycle it
// causes still has to be explained, so fatals are exempt from the mask.
#[test]
fn masked_fatal_still_logs_at_error() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    // own worker: error_reporting(0) is restored per cycle, so it would leak into later jobs
    let r = Rapira::start(Mode::Worker(fixture("error-levels-worker.php")))?;
    let h = r.handle()?;
    let _ = drain(h.handle_blocking(req("/?step=silent-fatal", "error-levels-worker.php"))?);
    drop(h);
    r.shutdown();

    let logged = captured();
    assert_eq!(
        last_error_levels(&logged, "SILENCED-FATAL"),
        vec![log::Level::Error],
        "a fatal stays visible however the script masks it (captured: {logged:?})"
    );
    Ok(())
}

// log_errors routes the diagnostic through the SAPI log callback too; both paths must agree.
// Assumes php_error_cb owns that callback: an extension that hooks it reports at its own level.
#[test]
fn logged_deprecation_stays_at_debug_on_both_paths() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Worker(fixture("error-levels-worker.php")))?;
    let h = r.handle()?;
    let (status, _) = drain(h.handle_blocking(req("/?step=logged", "error-levels-worker.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    let logged = captured();
    assert_eq!(
        log_callback_levels(&logged, "LOGGED-DEPRECATION"),
        vec![log::Level::Debug],
        "the log callback reports a deprecation at debug (captured: {logged:?})"
    );
    assert_eq!(
        last_error_levels(&logged, "LOGGED-DEPRECATION"),
        vec![log::Level::Debug],
        "so does the teardown slot (captured: {logged:?})"
    );
    Ok(())
}

#[test]
fn sapi_ini_entries_applied() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(req("/", "ini.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    assert!(
        body.contains("met=0"),
        "ini_entries must apply: max_execution_time=0 (got: {body:?})"
    );
    Ok(())
}

#[test]
fn status_code_does_not_leak_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("status-worker.php")))?;
    let h = r.handle()?;
    let (s1, _) = drain(h.handle_blocking(req("/?code=404", "status-worker.php"))?);
    let (s2, b2) = drain(h.handle_blocking(req("/", "status-worker.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(s1, 404, "explicit http_response_code(404)");
    assert_eq!(
        s2, 200,
        "default status must be 200, not the leaked 404 (body: {b2:?})"
    );
    Ok(())
}

#[test]
fn status_code_does_not_leak_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let (s1, _) = drain(h.handle_blocking(req("/", "status-404.php"))?);
    let (s2, _) = drain(h.handle_blocking(req("/", "hello.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(s1, 404);
    assert_eq!(
        s2, 200,
        "classic mode reuses SG on the thread; 404 must not leak"
    );
    Ok(())
}

#[test]
fn worker_finish_request_header_only() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("finish-request-headers-worker.php")))?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(req("/", "finish-request-headers-worker.php"))?);
    drop(h);
    r.shutdown();
    assert_eq!(status, 302);
    assert!(body.is_empty());
    Ok(())
}

#[test]
fn teardown_bailout_does_not_leave_gc_protected() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("gc-protect-worker.php")))?;
    let h = r.handle()?;
    let (_, b1) = drain(h.handle_blocking(req("/?seed=1", "gc-protect-worker.php"))?);
    assert!(
        b1.contains("seeded"),
        "req1 seeds + bails in teardown (got {b1:?})"
    );
    // Smoke coverage, not a strict guard for module.c's unconditional gc_protect(0):
    // every bailout recycles, and the recycle's php_request_startup resets
    // gc_protected before this request can observe it (so req2 would pass regardless).
    let (_, b2) = drain(h.handle_blocking(req("/?probe=1", "gc-protect-worker.php"))?);
    assert!(
        b2.contains("unprotected"),
        "worker recovers from a teardown bailout with GC unprotected (got {b2:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn error_get_last_cleared_between_worker_requests() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("last-error-worker.php")))?;
    let h = r.handle()?;
    // req1 raises a non-fatal warning (does NOT bail, so no recycle masks the reset)
    let (_, b1) = drain(h.handle_blocking(req("/?step=warn", "last-error-worker.php"))?);
    assert!(b1.contains("warned"), "req1 warns (got {b1:?})");
    // req2 must see a cleared last error (rapira_clear_last_error between jobs)
    let (_, b2) = drain(h.handle_blocking(req("/", "last-error-worker.php"))?);
    assert_eq!(
        b2, "clean",
        "error_get_last() must reset between jobs (got {b2:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn first_call_teardown_bailout_recycles_instead_of_serving_on_corrupt_state() -> anyhow::Result<()>
{
    let _guard = php_lock();
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let sentinel = tmp.join(format!("rapira_h2_sentinel_{pid}"));
    let boot = tmp.join(format!("rapira_h2_boot_{pid}"));
    let _ = std::fs::remove_file(&sentinel);
    let _ = std::fs::remove_file(&boot);

    let r = Rapira::start(Mode::Worker(fixture("h2-boot-bail-worker.php")))?;
    let h = r.handle()?;
    // The bootstrap's session save handler fatals on the first-call teardown flush.
    // Before the fix that bailout was swallowed and the job served in cycle 1 (count
    // "1"); after the fix it recycles, so the worker re-bootstraps and serves in
    // cycle 2 (count "2").
    let (_, body) = drain(h.handle_blocking(req("/", "h2-boot-bail-worker.php"))?);
    assert_eq!(
        body, "2",
        "first-call teardown bailout must recycle + re-bootstrap, not serve in cycle 1 (got {body:?})"
    );
    drop(h);
    r.shutdown();
    let _ = std::fs::remove_file(&sentinel);
    let _ = std::fs::remove_file(&boot);
    Ok(())
}

// A post-loop warning left in PG(last_error_message) trips the core_globals_dtor assertion
// at php_module_shutdown (main.c:2102).
#[test]
fn worker_error_after_loop_exits_cleanly() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("warn-after-loop-worker.php")))?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(req("/", "warn-after-loop-worker.php"))?);
    assert_eq!((status, body.as_str()), (200, "ok"));
    drop(h);
    r.shutdown(); // drop(r) joins the worker; the post-loop warning must be cleared before module shutdown
    Ok(())
}

#[test]
fn filter_raw_input_does_not_accumulate() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("filter-leak-worker.php")))?;
    let h = r.handle()?;
    let mem = |b: String| -> i64 {
        b.trim()
            .strip_prefix("mem=")
            .and_then(|s| s.parse().ok())
            .expect("mem= output")
    };
    let (_, first) = drain(h.handle_blocking(req("/?x=warmup", "filter-leak-worker.php"))?);
    if first.trim() == "skip" {
        drop(h);
        r.shutdown();
        return Ok(()); // PHP built without ext/filter
    }
    for i in 0..5 {
        let _ = drain(h.handle_blocking(req(&format!("/?x=w{i}"), "filter-leak-worker.php"))?);
    }
    let m1 = mem(drain(h.handle_blocking(req("/?x=base", "filter-leak-worker.php"))?).1);
    for i in 0..200 {
        let _ = drain(h.handle_blocking(req(&format!("/?x=v{i}"), "filter-leak-worker.php"))?);
    }
    let m2 = mem(drain(h.handle_blocking(req("/?x=end", "filter-leak-worker.php"))?).1);
    let leaked = m2 - m1;
    assert!(
        leaked < 32 * 1024,
        "raw input copies must not accumulate across jobs; {leaked} bytes grown (~90KB pre-fix)"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn worker_finish_request_flush_bailout_recycles() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("finish-request-bailout-worker.php")))?;
    let h = r.handle()?;
    let (s1, b1) = drain(h.handle_blocking(req("/?boom=0", "finish-request-bailout-worker.php"))?);
    let (s2, b2) = drain(h.handle_blocking(req("/?boom=1", "finish-request-bailout-worker.php"))?);
    let (s3, b3) = drain(h.handle_blocking(req("/?boom=0", "finish-request-bailout-worker.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(s1, 200);
    assert!(b1.contains("ok counter=1"), "req1 baseline (got: {b1:?})");
    // headers were never sent, so the classified job commits a 500 head; a
    // swallowed bailout closes the stream with no head at all (status 0)
    assert_eq!(
        s2, 500,
        "fatal during the finish_request flush must commit a 500 (got {s2}, {b2:?})"
    );
    assert!(
        !b2.contains("resumed-after-fatal"),
        "script must not resume past the bailout (got: {b2:?})"
    );
    assert_eq!(s3, 200, "worker must recover (got {s3})");
    assert!(
        b3.contains("ok counter=1"),
        "recycle resets statics (got: {b3:?})"
    );
    Ok(())
}
