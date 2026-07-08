use std::path::Path;

use integration_tests::{drain, fixture, php_lock_with_ini, req};
use php_sys::{Mode, Rapira};

// The observer API only registers at module startup, and zend_test's observer writes
// markers into the response body - so these run in their own process with their own ini,
// never the shared suite. Requires PHP built with --enable-zend-test; without it the
// observer API stays disabled and both tests degrade to plain worker runs.
fn observer_lock() -> std::sync::MutexGuard<'static, ()> {
    php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/observer.ini"
    )))
}

#[test]
fn observer_frames_balanced_after_bailout() -> anyhow::Result<()> {
    let _guard = observer_lock();
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
    Ok(())
}
