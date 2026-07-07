use std::io::Read;

use integration_tests::{drain, fixture, php_lock, req};
use php_sys::{Frame, Mode, Rapira, Request};

/// Body source returning at most one byte per read() call — legal `Read`
/// behavior that streaming bodies (pipes, chunked decoders) exhibit.
struct Trickle(std::io::Cursor<Vec<u8>>);

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let end = buf.len().min(1);
        self.0.read(&mut buf[..end])
    }
}

fn post(fixture_name: &str, body: Box<dyn Read + Send>, len: i64) -> Request {
    let mut r: Request = req("/", fixture_name);
    r.method = "POST".into();
    r.content_type = Some("text/plain".into());
    r.content_length = len;
    r.body = body;
    r
}

// php-src treats any short read_post() return as end-of-body
// (SG(post_read)=1, main/SAPI.c) - the callback must fill the buffer until
// real EOF or partial reads truncate the POST body.
#[test]
fn post_body_survives_partial_reads() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("input-worker.php")), 1)?;
    let h = r.handle()?;

    let payload = b"hello rapira post".to_vec(); // 17 bytes
    let len = payload.len() as i64;
    let request = post(
        "input-worker.php",
        Box::new(Trickle(std::io::Cursor::new(payload))),
        len,
    );
    let (status, body) = drain(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    assert!(
        body.contains("len=17") && body.contains("body=hello rapira post"),
        "php://input must see the whole trickled body (got: {body:?})"
    );
    Ok(())
}

// dropping the response receiver = client disconnect. PHP core ignores
// ub_write's return value, so the SAPI must raise
// php_handle_aborted_connection() itself: the handler's remaining work is cut
// short (default ignore_user_abort=0), and the ABORTED status must not leak
// into the next request.
#[test]
fn client_disconnect_aborts_request() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("abort-worker.php")), 1)?;
    let h = r.handle()?;

    // 64 chunks through the 16-slot frame channel: the worker blocks in
    // blocking_send, then fails it when the receiver goes away
    let mut rx = h.handle_blocking(req("/", "abort-worker.php"))?;
    let _head = rx.blocking_recv(); // response is live
    drop(rx); // client disconnects

    let (s2, b2) = drain(h.handle_blocking(req("/?probe=1", "abort-worker.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(s2, 200, "worker must survive the aborted request");
    assert!(
        b2.contains("done=0"),
        "work after the disconnect must not run (got: {b2:?})"
    );
    assert!(
        b2.contains("aborted=0"),
        "connection status must reset for the next request (got: {b2:?})"
    );
    Ok(())
}

// sapi_deactivate_module() only NULLs SG(request_info).request_body; in a
// resident worker request nothing reclaims the temp stream resource, so every
// POST grows EG(regular_list) until a sweep is in place.
#[test]
fn post_temp_streams_do_not_accumulate() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("resources-worker.php")), 1)?;
    let h = r.handle()?;

    let send = |h: &php_sys::RapiraHandle| -> anyhow::Result<i64> {
        let body = b"x=1".to_vec();
        let len = body.len() as i64;
        let (_, b) = drain(h.handle_blocking(post(
            "resources-worker.php",
            Box::new(std::io::Cursor::new(body)),
            len,
        ))?);
        b.split_once("streams=")
            .and_then(|(_, n)| n.trim().parse().ok())
            .ok_or_else(|| anyhow::anyhow!("fixture must print streams=N (got: {b:?})"))
    };

    let first = send(&h)?;
    send(&h)?;
    send(&h)?;
    let fourth = send(&h)?;
    drop(h);
    r.shutdown();

    assert_eq!(
        first, fourth,
        "stream resources must not accumulate across POST requests"
    );
    Ok(())
}

#[test]
fn https_server_vars() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("server-variables.php")), 1)?;
    let h = r.handle()?;
    let mut request = req("/", "server-variables.php");
    request.https = true;
    let (status, body) = drain(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    assert!(
        body.contains("[HTTPS] => on"),
        "TLS request must set $_SERVER['HTTPS']=on (got: {body:?})"
    );
    assert!(
        body.contains("[GATEWAY_INTERFACE] => CGI/1.1"),
        "got: {body:?}"
    );
    Ok(())
}

// classic mode runs userland set_exception_handler for uncaught
// throwables (zend_execute_scripts); the worker path must do the same. A
// handled exception is not an error: no 500, no scoreboard error.
// ----
// thread 'uncaught_throwable_reaches_exception_handler' (683998) panicked at crates/integration_tests/tests/general_tests.rs:161:5:
// set_exception_handler must receive the throwable (got: "<br />\n<b>Fatal error</b>:  Uncaught RuntimeException:
#[test]
fn uncaught_throwable_reaches_exception_handler() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("exception-handler-worker.php")), 1)?;
    let h = r.handle()?;
    let (s1, b1) = drain(h.handle_blocking(req("/", "exception-handler-worker.php"))?);
    let (s2, b2) = drain(h.handle_blocking(req("/", "exception-handler-worker.php"))?);
    drop(h);
    let snap = r.scoreboard();
    r.shutdown();

    assert_eq!(s1, 200);
    assert!(
        b1.contains("handled:boom") && !b1.contains("Uncaught"),
        "set_exception_handler must receive the throwable (got: {b1:?})"
    );
    assert_eq!(s2, 200);
    assert!(
        b2.contains("handled:boom"),
        "the handler persists on the worker (got: {b2:?})"
    );
    assert_eq!(snap.errors, 0, "a handled exception is not an engine error");
    Ok(())
}

// exactly one Head frame per response. With display_errors=0 an uncaught
// throw produces no output before the error path, so the rust-side 500 head
// and the teardown header flush must not both emit one.
#[test]
fn error_response_sends_exactly_one_head() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("throw-quiet-worker.php")), 1)?;
    let h = r.handle()?;

    let mut rx = h.handle_blocking(req("/", "throw-quiet-worker.php"))?;
    let (mut heads, mut status) = (0u32, 0u16);
    while let Some(frame) = rx.blocking_recv() {
        if let Frame::Head(head) = frame {
            heads += 1;
            status = head.status;
        }
    }
    drop(h);
    r.shutdown();

    assert_eq!(status, 500, "uncaught throw with display_errors=0 is a 500");
    assert_eq!(
        heads, 1,
        "exactly one head frame per response (got {heads})"
    );
    Ok(())
}

// RSHUTDOWN wraps php_session_flush alone in zend_try so a
// bailing save handler cannot skip the rest of the reset. Without the inner
// guard the bailed flush leaves the session active and the next request
// reuses the previous session id.
#[test]
fn session_reset_survives_bailing_save_handler() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("session-bailout-worker.php")), 1)?;
    let h = r.handle()?;
    let (_, b1) = drain(h.handle_blocking(req("/", "session-bailout-worker.php"))?);
    let (_, b2) = drain(h.handle_blocking(req("/", "session-bailout-worker.php"))?);
    drop(h);
    r.shutdown();

    let sid = |b: &str| {
        b.split_whitespace()
            .find_map(|t| t.strip_prefix("sid=").map(str::to_owned))
    };
    assert!(
        sid(&b1).is_some(),
        "req1 must start a session (got: {b1:?})"
    );
    assert_ne!(
        sid(&b1),
        sid(&b2),
        "a bailing save handler must not leave the previous session active (b1={b1:?}, b2={b2:?})"
    );
    Ok(())
}

#[test]
fn fatal_in_exception_handler_keeps_worker_alive() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(
        Mode::Worker(fixture("fatal-exception-handler-worker.php")),
        1,
    )?;
    let h = r.handle()?;
    let (s1, _) = drain(h.handle_blocking(req("/", "fatal-exception-handler-worker.php"))?);
    assert!(s1 == 200, "req1 must return a head, not hang (got {s1})");
    let (s2, _) = drain(h.handle_blocking(req("/", "fatal-exception-handler-worker.php"))?);
    assert!(
        s2 == 200 || s2 == 500,
        "worker must survive and serve req2 (got {s2})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

// #[test]
// fn teardown_from_foreign_thread() -> anyhow::Result<()> {
//     let _guard = php_lock();
//     let r = Rapira::start(Mode::Classic, 1)?;
//     let h = r.handle()?;
//     assert_eq!(drain(h.handle_blocking(req("/", "hello.php"))?).0, 200);
//     drop(h);
//     std::thread::spawn(move || drop(r)).join().unwrap();
//     Ok(())
// } <-- this test sshould not compile due to the PhantomData in Rapira (*const ())

// zend_test's observer writes markers to the response output, committing the head
// early and polluting the body - so this runs in its OWN binary with its OWN ini,
// never the shared suite. Requires PHP built with --enable-zend-test.
#[test]
fn observer_frames_balanced_after_bailout() -> anyhow::Result<()> {
    let _guard = php_lock();
    unsafe {
        std::env::set_var(
            "PHPRC",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/observer.ini"),
        );
    }
    let r = Rapira::start(Mode::Worker(fixture("observer-bailout.php")), 1)?;
    let h = r.handle()?;

    let (_, probe) = drain(h.handle_blocking(req("/?mode=ok", "observer-bailout.php"))?);
    if probe.contains("skip") {
        drop(h);
        r.shutdown();
        return Ok(()); // PHP built without --enable-zend-test
    }

    // outer() -> inner() -> trigger_error(E_USER_ERROR) bails; the closing tags
    // for both frames open at the bailout must still reach the response body.
    let (_, b1) = drain(h.handle_blocking(req("/?mode=fatal", "observer-bailout.php"))?);
    assert!(
        b1.contains("<inner>")
            && b1.contains("</inner>")
            && b1.contains("<outer>")
            && b1.contains("</outer>"),
        "observer begin+end must both fire for outer and inner despite the bailout (got {b1:?})"
    );

    // worker survives; the next request's frames are balanced too
    let (_, b2) = drain(h.handle_blocking(req("/?mode=ok", "observer-bailout.php"))?);
    assert!(
        b2.contains("</outer>") && b2.contains("ok"),
        "worker survives, next request balanced (got {b2:?})"
    );

    drop(h);
    r.shutdown();
    unsafe {
        std::env::set_var(
            "PHPRC",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/php.ini"),
        );
    }
    Ok(())
}

#[test]
fn in_user_include_flag_reset_between_requests() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("stuck-flag-worker.php")), 1)?;
    let h = r.handle()?;
    // req1: fatal inside the include-wrapper -> bailout strands in_user_include (returning proves no hang)
    let _ = drain(h.handle_blocking(req("/?step=boom", "stuck-flag-worker.php"))?);
    let (_, b2) = drain(h.handle_blocking(req("/", "stuck-flag-worker.php"))?);
    assert!(
        b2.contains("PROBE_OK"),
        "data:// (is_url) must not be rejected as an include -> in_user_include must reset (got {b2:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn fatal_backtrace_freed_between_requests() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("fatal-backtrace-worker.php")), 1)?;
    let h = r.handle()?;
    let mem = |b: String| -> i64 {
        b.trim()
            .strip_prefix("mem=")
            .and_then(|s| s.parse().ok())
            .expect("mem= output")
    };
    let b0 = mem(drain(h.handle_blocking(req("/?step=probe", "fatal-backtrace-worker.php"))?).1);
    // consumed fatal: execution continues, frame unwinds, backtrace is the sole ref to the 20MB
    let (_, boom) = drain(h.handle_blocking(req("/?step=boom", "fatal-backtrace-worker.php"))?);
    assert!(
        boom.contains("boomed"),
        "error consumed + execution continued (got {boom:?})"
    );
    let leaked =
        mem(drain(h.handle_blocking(req("/?step=probe", "fatal-backtrace-worker.php"))?).1) - b0;
    assert!(
        leaked < 5 * 1024 * 1024,
        "fatal backtrace must be freed between jobs; {leaked} bytes still pinned (~20MB pre-fix)"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn shutdown_function_fatal_recycles_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("shutdown-fatal-worker.php")), 1)?;
    let h = r.handle()?;
    let (_, b1) = drain(h.handle_blocking(req("/?boom=1", "shutdown-fatal-worker.php"))?);
    let (s2, b2) = drain(h.handle_blocking(req("/", "shutdown-fatal-worker.php"))?);
    drop(h);
    r.shutdown();
    assert!(b1.contains("ok counter=1"), "req1 baseline (got: {b1:?})");
    assert_eq!(s2, 200, "worker must survive (got {s2})");
    assert!(
        b2.contains("ok counter=1"),
        "fatal in shutdown fn must recycle, resetting statics (got: {b2:?})"
    );
    Ok(())
}

#[test]
fn client_disconnect_respects_ignore_user_abort() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("abort-ignore-worker.php")), 1)?;
    let h = r.handle()?;
    let mut rx = h.handle_blocking(req("/", "abort-ignore-worker.php"))?;
    let _head = rx.blocking_recv(); // response is live
    drop(rx); // client disconnects
    let (s2, b2) = drain(h.handle_blocking(req("/?probe=1", "abort-ignore-worker.php"))?);
    drop(h);
    r.shutdown();
    assert_eq!(s2, 200, "worker must survive the ignored abort");
    assert!(
        b2.contains("reached=1"),
        "work after disconnect must still run (got: {b2:?})"
    );
    assert!(
        b2.contains("aborted=0"),
        "connection status must reset (got: {b2:?})"
    );
    Ok(())
}
