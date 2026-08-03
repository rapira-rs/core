//! `\Rapira\get_dispatcher()` and the `Rapira\Exception` class set.

use php_sys::{Mode, Rapira};
use tests::{drain, php_lock, req};

/// Outside worker mode nothing feeds this process work, so the call must throw
/// the specific `NotInWorkerModeError` — catchable by its own name, branded
/// `RapiraThrowable` — and the `RuntimeException` family must be catchable by
/// its stock parent. Hierarchy is asserted through catch behavior: a wrong
/// parent CE passed to a registrar compiles fine and only fails here.
#[test]
fn get_dispatcher_outside_worker_mode_throws() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(req("/", "dispatcher/not-in-worker-mode.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(
        status, 200,
        "every throw in the script must be caught (body: {body:?})"
    );
    for line in [
        "class: Rapira\\Exception\\NotInWorkerModeError",
        "rapira: yes",
        "timeout-as-runtime: yes",
        "done",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    Ok(())
}
