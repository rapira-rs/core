use std::path::Path;

use integration_tests::{drain, php_lock, req, set_phprc};
use php_sys::{Mode, Rapira};

// <br />
// <b>Fatal error</b>:  Directive 'magic_quotes_gpc' is no longer available in PHP in
// <b>Unknown</b> on line <b>0</b><br />
#[test]
fn module_startup_failure_then_clean_restart() -> anyhow::Result<()> {
    let php = php_lock();
    set_phprc(
        &php,
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/php-removed.ini"
        )),
    );
    assert!(
        Rapira::start(Mode::Classic, 1).is_err(),
        "removed-directive ini must fail startup"
    );

    set_phprc(
        &php,
        Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/php.ini")),
    );
    let r = Rapira::start(Mode::Classic, 1)?; // must run a full module startup, not the early-return
    let h = r.handle()?;
    assert_eq!(drain(h.handle_blocking(req("/", "hello.php"))?).0, 200);
    drop(h);
    r.shutdown();
    Ok(())
}
