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

// The in-process harness drives jobs straight through RapiraHandle, bypassing
// pingora, so it can't observe the 404 — but it proves the config PHP declared at
// create_plugin_handler is marshaled to the Rust side and readable off the handle.
// (The pingora-side 404 behavior is the e2e test path_prefix_rejects_before_php.)
#[test]
fn handler_config_blob_is_delivered() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture("plugin-config-worker.php")))?;
    let h = r.handle()?;

    let (status, _) = drain(h.handle_blocking(req("/api/x", "plugin-config-worker.php"))?);
    assert_eq!(status, 200);

    // A served request proves the bootstrap ran, so the config is declared by now.
    // (Reading before this races the worker thread, which declares at thread start.)
    let blob = h.handler_config().expect("config declared at bootstrap");
    let json: serde_json::Value = serde_json::from_slice(&blob)?;
    assert_eq!(
        json["pathPrefix"], "/api",
        "the declared prefix reached Rust verbatim (got: {json})"
    );
    assert_eq!(
        json["info"]["name"], "http",
        "the plugin slot rides along (got: {json})"
    );

    drop(h);
    r.shutdown();
    Ok(())
}

// A config the factory cannot serialize, and one the front could never match, must
// both be refused at declaration time — and neither may disturb the config already
// declared. Without the encode check, the failed one ships a truncated JSON fragment
// that the plugin can only read as "nothing declared", silently dropping the prefix.
#[test]
fn refused_configs_leave_the_declared_one_intact() -> anyhow::Result<()> {
    let _guard = php_lock();
    let script = "plugin-config-errors-worker.php";
    let r = Rapira::start(Mode::Worker(fixture(script)))?;
    let h = r.handle()?;

    let (_, body) = drain(h.handle_blocking(req("/x?probe=utf8", script))?);
    assert!(
        body.contains("threw:cannot serialize"),
        "an unencodable config is refused (got: {body:?})"
    );

    let (_, body) = drain(h.handle_blocking(req("/x?probe=prefix", script))?);
    assert!(
        body.contains("threw:pathPrefix must start with '/'"),
        "a prefix the front cannot match is refused (got: {body:?})"
    );

    let blob = h
        .handler_config()
        .expect("the bootstrap config is still there");
    let json: serde_json::Value = serde_json::from_slice(&blob)?;
    assert_eq!(
        json["pathPrefix"], "/api",
        "a refused config never reached the plugin (got: {json})"
    );

    drop(h);
    r.shutdown();
    Ok(())
}
