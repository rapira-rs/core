use integration_tests::{drain, php_lock, req};
use php_sys::{Mode, Rapira, Request};

fn post(fixture_name: &str, query: &str, content_type: Option<&str>, body: Vec<u8>) -> Request {
    let mut r: Request = req(&format!("/{fixture_name}?{query}"), fixture_name);
    r.method = "POST".into();
    r.content_type = content_type.map(str::to_string);
    r.content_length = body.len() as i64;
    r.body = Box::new(std::io::Cursor::new(body));
    r
}

// POST form body parses into $_POST while the query string populates $_GET.
#[test]
fn post_superglobals_classic() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Classic, 1)?;
    let h = r.handle()?;
    let request = post(
        "post-superglobals.php",
        "foo=bar&baz=buz",
        Some("application/x-www-form-urlencoded"),
        b"bam=bam&some=10".to_vec(),
    );
    let (status, body) = drain(h.handle_blocking(request)?);
    drop(h);
    r.shutdown();

    assert_eq!(status, 200);
    for expected in [
        "'foo' => 'bar'",
        "'baz' => 'buz'",
        "'bam' => 'bam'",
        "'some' => '10'",
    ] {
        assert!(
            body.contains(expected),
            "missing {expected:?} (got: {body:?})"
        );
    }
    Ok(())
}
