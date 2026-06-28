use integration_tests::{drain_async, fixture, php_lock_async, req};
use php_sys::{Mode, Rapira};

#[tokio::test]
async fn hello_world_worker() -> anyhow::Result<()> {
    let _guard = php_lock_async().await;
    let r = Rapira::start(Mode::Worker(fixture("worker.php")), 1)?;
    let h = r.handle()?;
    let (_, body1) = drain_async(h.handle(req("/?x=1", "worker.php")).await?).await;
    assert!(
        body1.contains("Hello from worker, anonymous!"),
        "req1 baseline (got: {body1:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[tokio::test]
async fn worker_request_isolation() -> anyhow::Result<()> {
    let _guard = php_lock_async().await;
    let r = Rapira::start(Mode::Worker(fixture("leak-worker.php")), 1)?;
    let h = r.handle()?;
    let (_, body1) = drain_async(h.handle(req("/?x=1", "leak-worker.php")).await?).await;
    let (_, body2) = drain_async(h.handle(req("/?x=2", "leak-worker.php")).await?).await;
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

#[tokio::test]
async fn worker_survives_exit() -> anyhow::Result<()> {
    let _guard = php_lock_async().await;

    let r = Rapira::start(Mode::Worker(fixture("bailout-worker.php")), 1)?;
    let h = r.handle()?;
    let (s1, b1) = drain_async(h.handle(req("/?boom=0", "bailout-worker.php")).await?).await; // normal
    let (s2, b2) = drain_async(h.handle(req("/?boom=1", "bailout-worker.php")).await?).await; // exit(1) -> unwind-exit
    let (s3, b3) = drain_async(h.handle(req("/?boom=0", "bailout-worker.php")).await?).await; // worker must still serve

    assert_eq!(s1, 200);
    assert!(b1.contains("ok counter=1"), "req1 (got: {b1:?})");

    // exit() before any output: graceful unwind, empty body, default 200
    assert_eq!(
        s2, 200,
        "exit() is a graceful unwind, not a 500 (got status {s2}, body {b2:?})"
    );
    assert!(
        b2.is_empty(),
        "exit(1) before any output => empty body (got: {b2:?})"
    );

    // worker survived exit(), serves the next request
    assert_eq!(s3, 200, "worker must recover after exit() (got {s3})");
    assert!(
        b3.contains("ok counter=3"),
        "worker must survive exit() and serve the next request (got: {b3:?})"
    );
    drop(h);
    r.shutdown();
    Ok(())
}

#[tokio::test]
async fn fibers_stress_worker() -> anyhow::Result<()> {
    let _guard = php_lock_async().await;

    let r = Rapira::start(Mode::Worker(fixture("fibers-worker.php")), 1)?;
    let h = r.handle()?;
    let (status, body) = drain_async(h.handle(req("/", "fibers-worker.php")).await?).await;
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

#[tokio::test]
async fn worker_survives_teardown_bailout() -> anyhow::Result<()> {
    let _guard = php_lock_async().await;

    let r = Rapira::start(Mode::Worker(fixture("teardown-bailout-worker.php")), 1)?;
    let h = r.handle()?;
    let (s1, b1) = drain_async(
        h.handle(req("/?boom=0", "teardown-bailout-worker.php"))
            .await?,
    )
    .await; // normal
    let (s2, b2) = drain_async(
        h.handle(req("/?boom=1", "teardown-bailout-worker.php"))
            .await?,
    )
    .await; // bail in teardown
    let (s3, b3) = drain_async(
        h.handle(req("/?boom=0", "teardown-bailout-worker.php"))
            .await?,
    )
    .await; // must still serve

    assert_eq!(s1, 200);
    assert!(b1.contains("ok counter=1"), "req1 baseline (got: {b1:?})");

    // teardown bailout commits a 200 head before the 500; buffered body lost
    assert_eq!(
        s2, 200,
        "teardown-flush bailout commits a 200 head (got {s2}, body {b2:?})"
    );
    assert!(
        b2.is_empty(),
        "buffered body is lost to the bailout (got: {b2:?})"
    );

    // worker survived the teardown bailout; counter 1 -> 2 (bail) -> 3
    assert_eq!(
        s3, 200,
        "worker must recover after a teardown bailout (got {s3})"
    );
    assert!(
        b3.contains("ok counter=3"),
        "worker must survive the teardown bailout and serve the next request (got: {b3:?})"
    );

    drop(h);
    r.shutdown();
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn many_producers_test() -> anyhow::Result<()> {
    let _guard = php_lock_async().await;

    let r = Rapira::start(Mode::Worker(fixture("worker.php")), 10)?;

    let producers: Vec<_> = (0..24)
        .map(|t| {
            let h: php_sys::RapiraHandle = r.handle().unwrap();
            tokio::spawn(async move {
                for i in 0..256 {
                    let name: String = format!("t{t}-r{i}");
                    let rx = h
                        .handle(req(&format!("/?name={name}"), "worker.php"))
                        .await
                        .expect("ruuuun!");
                    let (status, body) = drain_async(rx).await;
                    assert_eq!(
                        status, 200,
                        "worker must serve (got {status}, body {body:?})"
                    );
                    assert!(
                        body.contains(&format!("Hello from worker, {name}!")),
                        "worker must serve (got: {body:?})"
                    );
                }
            })
        })
        .collect::<Vec<_>>();

    for p in producers {
        // re-raise the producer's *original* assertion (message + location) instead of
        // masking it behind a generic JoinError, as a bare `.expect()` would.
        if let Err(e) = p.await {
            std::panic::resume_unwind(e.into_panic());
        }
    }

    r.shutdown();
    Ok(())
}

#[tokio::test]
async fn worker_basic_auth() -> anyhow::Result<()> {
    let _guard = php_lock_async().await;

    let r = Rapira::start(Mode::Worker(fixture("auth-worker.php")), 1)?;
    let h = r.handle()?;

    // Authorization: Basic base64("user:pass")
    let mut with_auth = req("/", "auth-worker.php");
    with_auth
        .headers
        .push(("Authorization".into(), "Basic dXNlcjpwYXNz".into()));
    let (s_auth, b_auth) = drain_async(h.handle(with_auth).await?).await;

    // no Authorization header on the next request
    let (s_none, b_none) = drain_async(h.handle(req("/", "auth-worker.php")).await?).await;

    drop(h);
    r.shutdown();

    assert_eq!(s_auth, 200);
    assert!(
        b_auth.contains("user=user") && b_auth.contains("pass=pass"),
        "Basic auth must populate PHP_AUTH_USER/PW (got: {b_auth:?})",
    );

    // proves auth doesn't leak across requests
    assert_eq!(s_none, 200);
    assert!(
        b_none.contains("user=- pass=-"),
        "no auth header -> no PHP_AUTH vars (got: {b_none:?})",
    );

    Ok(())
}

#[tokio::test]
async fn server_variables() -> anyhow::Result<()> {
    let _guard = php_lock_async().await;

    let r: Rapira = Rapira::start(Mode::Worker(fixture("server-variables.php")), 1)?;
    let h: php_sys::RapiraHandle = r.handle()?;

    let mut request: php_sys::Request =
        req("/server-variables.php?foo=a&bar=b", "server-variables.php");
    request.method = "POST".into();
    request.content_type = Some("text/plain".into());
    request.content_length = 3;
    request.body = Box::new(std::io::Cursor::new(b"foo".to_vec()));
    request
        .headers
        .push(("Authorization".into(), "Basic dmFsZXJ5OnBhc3N3b3Jk".into()));

    let (status, body) = drain_async(h.handle(request).await?).await;
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    for expected in [
        "[REQUEST_METHOD] => POST",
        "[QUERY_STRING] => foo=a&bar=b",
        "[REQUEST_URI] => /server-variables.php?foo=a&bar=b",
        "[CONTENT_TYPE] => text/plain",
        "[CONTENT_LENGTH] => 3",
        "[REMOTE_ADDR] => 127.0.0.1",
        "[SERVER_NAME] => localhost",
        "[SERVER_PORT] => 8080",
        "[SERVER_PROTOCOL] => HTTP/1.1",
        "[SERVER_SOFTWARE] => Rapira",
        "[HTTP_AUTHORIZATION] => Basic dmFsZXJ5OnBhc3N3b3Jk",
        "[PHP_AUTH_USER] => valery",
        "[PHP_AUTH_PW] => password",
    ] {
        assert!(
            body.contains(expected),
            "$_SERVER missing {expected:?} (got: {body:?})"
        );
    }
    Ok(())
}

#[tokio::test]
async fn worker_finish_request() -> anyhow::Result<()> {
    let _guard = php_lock_async().await;

    let r = Rapira::start(Mode::Worker(fixture("finish-request-worker.php")), 1)?;
    let h = r.handle()?;

    let (s1, b1) = drain_async(h.handle(req("/", "finish-request-worker.php")).await?).await;
    let (s2, b2) = drain_async(h.handle(req("/", "finish-request-worker.php")).await?).await;

    drop(h);
    r.shutdown();

    // output BEFORE rapira_finish_request() is flushed to the client
    assert_eq!(s1, 200);
    assert!(
        b1.contains("count=0") && b1.contains("BEFORE"),
        "pre-finish output must reach the client (got: {b1:?})"
    );
    // output AFTER it is dropped — finish() cleared the response sender (tx = None)
    assert!(
        !b1.contains("AFTER"),
        "post-finish output must NOT reach the client (got: {b1:?})"
    );

    // worker survived, and the post-response work (State::$n++) ran after the stream closed
    assert_eq!(s2, 200);
    assert!(
        b2.contains("count=1"),
        "work after rapira_finish_request() must still execute (got: {b2:?})"
    );
    assert!(
        !b2.contains("AFTER"),
        "post-finish output stays dropped on the next request too (got: {b2:?})"
    );

    Ok(())
}

#[tokio::test]
async fn getenv_worker() -> anyhow::Result<()> {
    let _guard = php_lock_async().await;
    unsafe {
        std::env::set_var("FOO", "BAR");
    }
    let r = Rapira::start(Mode::Worker(fixture("env-worker.php")), 1)?;
    let h = r.handle()?;
    let (status, body) = drain_async(h.handle(req("/", "env-worker.php")).await?).await;
    drop(h);
    r.shutdown();
    assert_eq!(status, 200);
    assert!(body.contains("BAR"), "expected FOO=BAR (got: {body:?})");
    Ok(())
}

#[tokio::test]
async fn scoreboard_counts_worker() -> anyhow::Result<()> {
    let _guard = php_lock_async().await;
    let r = Rapira::start(Mode::Worker(fixture("throw-worker.php")), 1)?;
    let h = r.handle()?;
    let _ = drain_async(h.handle(req("/?boom=0", "throw-worker.php")).await?).await; // ok
    let _ = drain_async(h.handle(req("/?boom=0", "throw-worker.php")).await?).await; // ok
    let _ = drain_async(h.handle(req("/?boom=1", "throw-worker.php")).await?).await; // throw -> error
    drop(h);
    let snap = r.scoreboard();
    r.shutdown();

    assert_eq!(snap.handled, 3, "3 requests handled");
    assert_eq!(snap.errors, 1, "one engine error (uncaught throw)");
    assert_eq!(snap.workers.len(), 1);
    assert_eq!(snap.workers[0].handled, 3);
    assert_eq!(snap.workers[0].errors, 1);
    Ok(())
}
