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
