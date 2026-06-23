use integration_tests::{drain, fixture, req};
use php_sys::{Mode, Rapira};
use std::sync::Mutex;

static PHP_LOCK: Mutex<()> = Mutex::new(());

// this test works on both zts and nts
#[test]
fn hello_world_classic() -> anyhow::Result<()> {
    let _guard = PHP_LOCK.lock().unwrap();
    let r = Rapira::boot(Mode::Classic, 1)?; // 1 thread => same interpreter both reqs
    let h = r.handle();
    let (_, body1) = drain(h.dispatch_blocking(req("/?x=1", "hello.php"))?);
    assert!(
        body1.contains("Hello, anonymous!") && body1.contains("Method: GET"),
        "req1 baseline (got: {body1:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn fibers_stress_classic() -> anyhow::Result<()> {
    let _guard = PHP_LOCK.lock().unwrap();
    let r = Rapira::boot(Mode::Classic, 1)?;
    let h = r.handle();
    let (status, body) = drain(h.dispatch_blocking(req("/", "fibers.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(
        status, 200,
        "fiber script must compile + run without a stack-guard fatal (got {status}, body {body:?})"
    );
    assert!(
        body.contains("fibers ok sum=226644"),
        "fibers must complete with the correct total (got: {body:?})"
    );
    Ok(())
}

#[test]
fn hello_world_worker() -> anyhow::Result<()> {
    let _guard = PHP_LOCK.lock().unwrap();
    let r = Rapira::boot(Mode::Worker(fixture("worker.php")), 1)?; // 1 thread => same interpreter both reqs
    let h = r.handle();
    let (_, body1) = drain(h.dispatch_blocking(req("/?x=1", "worker.php"))?);
    assert!(
        body1.contains("Hello from worker, anonymous!"),
        "req1 baseline (got: {body1:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn worker_request_isolation() -> anyhow::Result<()> {
    let _guard = PHP_LOCK.lock().unwrap();
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
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn worker_survives_exit() -> anyhow::Result<()> {
    let _guard = PHP_LOCK.lock().unwrap();

    let r = Rapira::boot(Mode::Worker(fixture("bailout-worker.php")), 1)?;
    let h = r.handle();
    let (s1, b1) = drain(h.dispatch_blocking(req("/?boom=0", "bailout-worker.php"))?); // normal
    let (s2, b2) = drain(h.dispatch_blocking(req("/?boom=1", "bailout-worker.php"))?); // exit(1) -> unwind-exit
    let (s3, b3) = drain(h.dispatch_blocking(req("/?boom=0", "bailout-worker.php"))?); // worker must still serve

    assert_eq!(s1, 200);
    assert!(b1.contains("ok counter=1"), "req1 (got: {b1:?})");

    // exit() is a graceful unwind in 8.4+, not a bailout: no output before exit => empty body, default 200.
    assert_eq!(
        s2, 200,
        "exit() is a graceful unwind, not a 500 (got status {s2}, body {b2:?})"
    );
    assert!(
        b2.is_empty(),
        "exit(1) before any output => empty body (got: {b2:?})"
    );

    // THE KEY GUARANTEE: the worker survived the exit() and serves the next request cleanly.
    assert_eq!(s3, 200, "worker must recover after exit() (got {s3})");
    assert!(
        b3.contains("ok counter=3"),
        "worker must survive exit() and serve the next request (got: {b3:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[test]
fn fibers_stress_worker() -> anyhow::Result<()> {
    let _guard = PHP_LOCK.lock().unwrap();

    let r = Rapira::boot(Mode::Worker(fixture("fibers-worker.php")), 1)?;
    let h = r.handle();
    let (status, body) = drain(h.dispatch_blocking(req("/", "fibers-worker.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(
        status, 200,
        "fiber script must compile + run without a stack-guard fatal (got {status}, body {body:?})"
    );
    assert!(
        body.contains("fibers ok sum=226644"),
        "fibers must complete with the correct total (got: {body:?})"
    );
    Ok(())
}
