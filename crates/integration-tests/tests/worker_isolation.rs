//! One boot per process (PHP embed init is a process-global singleton).
use integration_tests::{drain, fixture, req};
use php_sys::{Mode, Rapira};

#[test]
fn worker_request_isolation() -> anyhow::Result<()> {
    let r = Rapira::boot(Mode::Worker(fixture("leak-worker.php")), 1)?; // 1 thread => same interpreter both reqs
    let h = r.handle();
    let (_, body1) = drain(h.dispatch_blocking(req("/?x=1", "leak-worker.php"))?);
    let (_, body2) = drain(h.dispatch_blocking(req("/?x=2", "leak-worker.php"))?);
    assert!(
        body1.contains("counter=1") && body1.contains("session=clean"),
        "req1 baseline (got: {body1:?})"
    );
    assert!(
        body2.contains("session=clean"),
        "$_SESSION must reset between requests (got: {body2:?})"
    );
    assert!(
        body2.contains("counter=2"),
        "static class props persist across requests by design (got: {body2:?})"
    );
    drop(h); // shutdown()'s dispatcher.join() needs every RapiraHandle dropped, else intake never EOFs
    r.shutdown();
    Ok(())
}
