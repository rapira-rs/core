use integration_tests::{drain, fixture, php_lock, req};
use php_sys::{Mode, Rapira};

#[test]
fn plugin_handler_serves_requests() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("plugin-handler-worker.php")))?;
    let h = r.handle()?;

    let (status, body) = drain(h.handle_blocking(req("/", "plugin-handler-worker.php"))?);
    assert_eq!(status, 200, "baseline (body {body:?})");
    assert!(
        body.contains("plugin=http"),
        "the config names the slot it targets (got: {body:?})"
    );
    // The counters come from this worker's live scoreboard slot, so the request
    // reading them sees itself as active and its own completion not yet counted.
    assert!(
        body.contains("state=active") && body.contains("handled=0"),
        "getInfo reads the live slot (got: {body:?})"
    );

    // A throwing handler must not take the loop down: the next request still serves.
    // (Status stays 200 here: display_errors is on, so PHP's fatal output commits
    // the head before the error path can - see general_tests::error_response_sends_exactly_one_head.)
    let (_, boom) = drain(h.handle_blocking(req("/?boom=1", "plugin-handler-worker.php"))?);
    assert!(
        boom.contains("boom"),
        "the throw reached the client (got: {boom:?})"
    );
    let (after, body) = drain(h.handle_blocking(req("/", "plugin-handler-worker.php"))?);
    assert_eq!(after, 200, "the loop survives the throw (body {body:?})");
    assert!(
        body.contains("handled=2"),
        "counters advance across requests (got: {body:?})"
    );

    drop(h);
    let snap = r.scoreboard();
    r.shutdown();
    assert_eq!(snap.handled, 3);
    assert_eq!(snap.errors, 1, "only the throw errored");
    Ok(())
}

#[test]
fn plugin_handler_class_shape() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("plugin-handler-shape-worker.php")))?;
    let h = r.handle()?;
    let (status, body) = drain(h.handle_blocking(req("/", "plugin-handler-shape-worker.php"))?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200, "the shape probe ran (body {body:?})");
    for expected in [
        "final=1",
        "abstract=1",
        "readonly=1",
        // A readonly *class* does not imply readonly properties for internal
        // classes - the engine only applies that rule in the compiler - so these
        // two are what prove the generated flags are the ones we meant.
        "prop-readonly=1",
        "write=blocked",
        "instanceof=1",
        "ctor=blocked ctor=blocked",
    ] {
        assert!(body.contains(expected), "{expected} (got: {body:?})");
    }
    Ok(())
}

#[test]
fn plugin_handler_refused_in_classic_mode() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic)?;
    let h = r.handle()?;
    let (_, body) = drain(h.handle_blocking(req("/", "plugin-handler-classic.php"))?);
    drop(h);
    r.shutdown();

    assert!(
        body.contains("Rapira\\RapiraException: plugin handlers require worker mode"),
        "the factory refuses, naming the reason (got: {body:?})"
    );
    Ok(())
}
