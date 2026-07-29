use php_sys::{Mode, Rapira};
use tests::{drain, fixture, php_lock, req};

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
    // The fused flavor runs the worker in this process, so the slot's pid is ours.
    assert!(
        body.contains(&format!("pid={}", std::process::id())),
        "getInfo reports this worker's pid (got: {body:?})"
    );
    assert!(
        body.contains("errors=0") && body.contains("recycles=0") && body.contains("restarts=0"),
        "a clean first request has nothing to report (got: {body:?})"
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
        body.contains("handled=2") && body.contains("errors=1"),
        "counters advance across requests (got: {body:?})"
    );
    // An uncaught throw is an error response, not corrupt executor state.
    assert!(
        body.contains("recycles=0") && body.contains("restarts=0"),
        "the throw needed no recycle (got: {body:?})"
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
        // HttpHandler, RuntimeInfo, PluginInfo: every class a factory owns.
        "ctor=blocked ctor=blocked ctor=blocked",
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

// A config class no plugin claims is the only error the dispatch itself can raise, and
// userland can reach it by subclassing the abstract base.
#[test]
fn unknown_config_class_is_refused() -> anyhow::Result<()> {
    let _guard = php_lock();
    let script = "plugin-unknown-config-worker.php";
    let r = Rapira::start(Mode::Worker(fixture(script)))?;
    let h = r.handle()?;

    let (_, body) = drain(h.handle_blocking(req("/", script))?);
    drop(h);
    r.shutdown();

    assert!(
        body.contains("threw:no plugin handler for config UnknownConfig"),
        "the factory names the class it cannot serve (got: {body:?})"
    );
    Ok(())
}

// A userland subclass of the abstract PluginHandler inherits the no-ctor handler, so
// `new` throws. It must not then run __destruct over the half-built instance.
#[test]
fn blocked_construction_skips_the_destructor() -> anyhow::Result<()> {
    let _guard = php_lock();
    let script = "plugin-handler-subclass-worker.php";
    let r = Rapira::start(Mode::Worker(fixture(script)))?;
    let h = r.handle()?;

    let (_, body) = drain(h.handle_blocking(req("/", script))?);
    drop(h);
    r.shutdown();

    assert!(
        body.contains("threw:Cannot directly construct Foo"),
        "the subclass inherits the no-ctor handler (got: {body:?})"
    );
    assert!(
        !body.contains("DESTRUCT"),
        "__destruct must not run on a never-constructed object (got: {body:?})"
    );
    Ok(())
}
