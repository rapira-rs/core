use std::path::Path;

use php_sys::{Mode, Rapira};
use tests::{drain, php_lock, req, set_phprc};

// A removed ini directive is a module-startup fatal, which is what makes Rapira::start fail;
// the follow-up clean-ini start must then run a full module startup.
#[test]
fn module_startup_failure_then_clean_restart() -> anyhow::Result<()> {
    let php = php_lock();
    set_phprc(
        &php,
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/ini/failboot_tests/php-removed.ini"
        )),
    );
    assert!(
        Rapira::start(Mode::Classic).is_err(),
        "removed-directive ini must fail startup"
    );

    set_phprc(
        &php,
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/ini/shared/php.ini"
        )),
    );
    let r = Rapira::start(Mode::Classic)?; // must run a full module startup, not the early-return
    let h = r.handle()?;
    assert_eq!(
        drain(h.handle_blocking(req("/", "shared/hello.php"))?).0,
        200
    );
    drop(h);
    r.shutdown();
    Ok(())
}
