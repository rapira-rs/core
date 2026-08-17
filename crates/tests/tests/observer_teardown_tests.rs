use std::path::Path;

use php_sys::{Mode, Rapira};
use tests::{drain, fixture, php_lock_with_ini, req};

// A bailing save handler bails inside rapira_reset_session. The longjmp skips
// the observer end handlers of every frame it abandons; rapira_request_teardown
// must close them, or EG(current_observed_frame) points at VM-stack slots the
// worker loop frees and zend_observer_fcall_end_all() walks into them at cycle
// end.
//
// Own binary, own ini: PHPRC is process-global and zend_test's markers must
// stay off - printing them hides the fault. Needs --enable-zend-test.
// https://github.com/php/php-src/pull/5857
#[test]
fn bailing_save_handler_leaves_no_dangling_observer_frame() -> anyhow::Result<()> {
    let _guard = php_lock_with_ini(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/ini/observer_teardown_tests/observer-quiet.ini"
    )));
    let r = Rapira::start(Mode::Worker(fixture("shared/session-bailout-worker.php")))?;
    let h = r.handle()?;

    // each job bails in teardown and recycles; the cycle end walks the observer chain
    for _ in 0..3 {
        let (_, body) = drain(h.handle_blocking(req("/", "shared/session-bailout-worker.php"))?);
        assert!(
            body.contains("sid="),
            "worker must keep serving (got {body:?})"
        );
    }

    drop(h);
    r.shutdown(); // must not fault
    Ok(())
}
