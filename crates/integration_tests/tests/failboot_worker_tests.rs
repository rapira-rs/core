use integration_tests::{drain, fixture, php_lock, req};
use php_sys::{Mode, Rapira};
use std::sync::mpsc;
use std::time::Duration;

// A6: a worker script that fatals before its `rapira_handle_request` loop can
// never read the intake channel from PHP. The Rust boot-failure drain must
// (a) answer the queued job with 503 and (b) observe channel closure so
// `Drop for Rapira` returns instead of joining a worker that retries the boot
// forever. Pre-fix (retry-forever loop) BOTH the response and Drop hang.
#[test]
fn failboot_worker_serves_503_and_drops_cleanly() -> anyhow::Result<()> {
    let _guard = php_lock();
    let (done_tx, done_rx) = mpsc::sync_channel::<(u16, String)>(1);

    // Rapira is !Send: build, use, and drop it entirely on one thread. The test
    // thread only enforces a deadline so a regression fails loudly instead of
    // hanging the whole suite.
    let scenario = std::thread::spawn(move || -> anyhow::Result<()> {
        let r = Rapira::start(Mode::Worker(fixture("failboot-worker.php")), 1)?;
        let h = r.handle()?;
        let rx = h.handle_blocking(req("/", "failboot-worker.php"))?;
        drop(h); // last non-Rapira intake sender — lets the channel close on drop(r)
        let (status, body) = drain(rx); // pre-fix: blocks forever (no 503 ever sent)
        drop(r); // pre-fix: joins a worker stuck in the retry loop -> hangs
        let _ = done_tx.send((status, body));
        Ok(())
    });

    let (status, _body) = done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("broken worker black-holed the request or hung Drop (A6 regression)");
    assert_eq!(status, 503, "a boot-failed worker must 503 the queued job");
    scenario.join().expect("scenario thread panicked")?;
    Ok(())
}
