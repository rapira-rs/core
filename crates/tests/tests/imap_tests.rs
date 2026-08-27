use std::path::Path;

use tests::{assert_skip_allowed, captured, init_log_capture, run_worker};

// ext/imap (PECL imap) reads default_socket_timeout at module startup only. This suite
// runs in its own process with its own ini. The ini value 17 differs from the built-in 60.
const IMAP_INI: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/ini/imap_tests/imap.ini"
);

fn run(name: &str, uris: &[&str]) -> anyhow::Result<Vec<(u16, String)>> {
    run_worker(name, uris, Some(Path::new(IMAP_INI)))
}

/// MINIT stores FG(default_socket_timeout) into c-client (PECL imap php_imap.c,
/// SET_*TIMEOUT). Nothing reads the ini again, so a per-request ini_set changes only
/// PHP's ini view. A real connect uses the same value: no request can change the imap
/// socket timeouts. Under the server the value comes from the master's php.ini.
#[test]
fn imap_timeout_is_snapshotted_at_minit() -> anyhow::Result<()> {
    let out = run("imap_tests/imap-timeout-worker.php", &["/", "/"])?;
    if out[0].1 == "skip" {
        assert_skip_allowed("imap_tests/imap-timeout-worker.php");
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
        "RINIT does not re-read the ini, so the next request sees the same snapshot (got: {:?})",
        out[1]
    );
    Ok(())
}

/// rapira reloads ext/imap per job (RELOAD_MODULES in php_sys/module.c): RSHUTDOWN
/// frees the error stack and RINIT resets it, as under php-fpm. An undrained error
/// must not leak into the next request on the same interpreter.
#[test]
fn imap_error_stack_does_not_leak_between_requests() -> anyhow::Result<()> {
    let out = run("imap_tests/imap-errors-worker.php", &["/?step=leak", "/"])?;
    if out[0].1 == "skip" {
        assert_skip_allowed("imap_tests/imap-errors-worker.php");
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

/// The per-job RSHUTDOWN reports each undrained entry as an E_NOTICE (free_errorlist
/// in PECL imap php_imap.c). The notice reaches the operator log through the SAPI
/// log_message hook, so an application that never drains imap_errors() is visible.
#[test]
fn imap_undrained_error_reaches_the_log() -> anyhow::Result<()> {
    init_log_capture();
    captured().clear();
    let out = run("imap_tests/imap-errors-worker.php", &["/?step=leak", "/"])?;
    if out[0].1 == "skip" {
        assert_skip_allowed("imap_tests/imap-errors-worker.php");
        return Ok(());
    }
    assert_eq!(
        (out[1].0, out[1].1.as_str()),
        (200, "imap:errors:empty"),
        "the follow-up must see a reset stack (got: {:?})",
        out[1]
    );
    let seen = captured()
        .iter()
        .any(|c| c.target == "php" && c.message.contains("invalid remote specification (errflg="));
    assert!(
        seen,
        "the RSHUTDOWN notice for the undrained error must reach the php log target"
    );
    Ok(())
}
