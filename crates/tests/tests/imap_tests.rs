use std::path::Path;

use php_sys::{Mode, Rapira};
use tests::{drain, fixture, php_lock_with_ini, req};

// ext/imap reads default_socket_timeout at module startup only. This suite runs in its
// own process with its own ini. The ini value 17 differs from the built-in 60.
fn imap_lock() -> std::sync::MutexGuard<'static, ()> {
    php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/imap_tests/imap.ini"
    )))
}

fn run(name: &str, uris: &[&str]) -> anyhow::Result<Vec<(u16, String)>> {
    let _guard = imap_lock();
    let r = Rapira::start(Mode::Worker(fixture(name)))?;
    let h = r.handle();
    let mut out = Vec::with_capacity(uris.len());
    for uri in uris {
        out.push(drain(h.handle_blocking(req(uri, name))?));
    }
    drop(h);
    r.shutdown();
    Ok(out)
}

/// MINIT stores FG(default_socket_timeout) into c-client (php_imap.c, SET_*TIMEOUT).
/// Nothing reads the ini again. Under rapira the master's php.ini fixes the value, and
/// a per-request ini_set changes only PHP's ini view. A real connect uses the same
/// value: no request can change the imap socket timeouts.
#[test]
fn imap_timeout_is_snapshotted_at_minit() -> anyhow::Result<()> {
    let out = run("imap_tests/imap-timeout-worker.php", &["/", "/"])?;
    if out[0].1 == "skip" {
        return Ok(());
    }
    let expected = "imap:open=17:after_ini_set=17:read=17:ini=5";
    assert_eq!(
        (out[0].0, out[0].1.as_str()),
        (200, expected),
        "c-client must keep the MINIT snapshot after ini_set (got: {:?})",
        out[0]
    );
    assert_eq!(
        (out[1].0, out[1].1.as_str()),
        (200, expected),
        "RINIT touches only the error stacks, so the next request sees the same snapshot (got: {:?})",
        out[1]
    );
    Ok(())
}

/// RINIT resets the imap error stack and RSHUTDOWN frees it. An undrained error must
/// not leak into the next request on the same interpreter.
#[test]
fn imap_error_stack_does_not_leak_between_requests() -> anyhow::Result<()> {
    let out = run("imap_tests/imap-errors-worker.php", &["/?step=leak", "/"])?;
    if out[0].1 == "skip" {
        return Ok(());
    }
    assert_eq!(
        (out[0].0, out[0].1.as_str()),
        (200, "imap:leaked:1"),
        "the leak request must push one entry and keep it undrained (got: {:?})",
        out[0]
    );
    assert_eq!(
        (out[1].0, out[1].1.as_str()),
        (200, "imap:errors:empty"),
        "the next request must find an empty error stack (got: {:?})",
        out[1]
    );
    Ok(())
}
