//! The dispatcher receive loop and the buffered Exchange verbs in
//! dispatcher mode: `receive()`/`tryReceive()` semantics, head/body
//! finalization discipline, and the `Rapira\Http\Request` field mapping.
//!
//! Locals are declared `r` before `h` on purpose: locals drop in reverse
//! order, so `h`'s Sender dies first and a failing assertion cannot hang the
//! suite; dropping both is also what delivers `ClosedException` to a parked
//! `receive()`.

use php_sys::{Frame, Mode, Rapira};
use std::io::Cursor;
use tests::{captured, drain, fixture, init_log_capture, php_lock, req};

fn drain_frame(mut rx: tokio::sync::mpsc::Receiver<Frame>) -> Option<Frame> {
    rx.blocking_recv()
}

fn header_value(frame: &Frame, name: &str) -> Option<String> {
    frame
        .head
        .as_ref()?
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
}

/// Boot `verbs-worker.php`, serve one probe request, tear down — the shared
/// shape of every single-probe test.
fn verbs_probe(query: &str) -> anyhow::Result<(u16, String)> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/verbs-worker.php")))?;
    let h = r.handle()?;
    let out = drain(h.handle_blocking(req(query, "dispatcher/verbs-worker.php"))?);
    drop(h);
    r.shutdown();
    Ok(out)
}

/// Two sequential units through the echo loop: explicit heads, per-request
/// bodies, and the worker surviving between them. Dropping the handle and the
/// pool afterwards must land as `ClosedException` in the parked `receive()`,
/// which the fixture reports as a `drained` app record.
#[test]
fn exchange_serves_sequential_requests() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/echo-loop-worker.php")))?;
    let h = r.handle()?;

    let frame = drain_frame(h.handle_blocking(req("/first", "dispatcher/echo-loop-worker.php"))?)
        .expect("first unit must seal a frame");
    assert_eq!(frame.head.as_ref().map(|hd| hd.status), Some(200));
    assert_eq!(
        header_value(&frame, "x-rapira-target").as_deref(),
        Some("/first")
    );
    assert_eq!(
        String::from_utf8_lossy(&frame.body),
        "method=GET body=",
        "empty request body echoes empty"
    );

    let mut rq2 = req("/second", "dispatcher/echo-loop-worker.php");
    rq2.body = Box::new(Cursor::new(b"two".to_vec()));
    rq2.content_length = 3;
    let frame = drain_frame(h.handle_blocking(rq2)?).expect("second unit must seal a frame");
    assert_eq!(
        header_value(&frame, "x-rapira-target").as_deref(),
        Some("/second")
    );
    assert_eq!(String::from_utf8_lossy(&frame.body), "method=GET body=two");

    drop(h);
    r.shutdown(); // joins the worker: receive() saw closure, the script wound down

    let drained = captured()
        .iter()
        .filter(|c| c.target == "app" && c.message == "drained")
        .count();
    assert_eq!(
        drained, 1,
        "ClosedException must reach the fixture exactly once"
    );
    Ok(())
}

/// `tryReceive()`/`receive(0)`/`receive(50ms)` against an empty-but-open
/// channel: no handle is ever created, so no job can precede the probes. The
/// pool must stay alive until the fixture has logged — dropping it early would
/// turn Empty into Closed and fail the probes.
#[test]
fn recv_probes_on_an_empty_channel() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Dispatcher(fixture(
        "dispatcher/recv-probes-worker.php",
    )))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if captured()
            .iter()
            .any(|c| c.target == "app" && c.message == "recv-probes")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "recv-probes record never appeared"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    r.shutdown();

    let contexts: Vec<String> = captured()
        .iter()
        .filter(|c| c.target == "app" && c.message == "recv-probes")
        .map(|c| c.context.clone())
        .collect();
    assert_eq!(contexts.len(), 1, "one probe record (got {contexts:?})");
    for fragment in [
        r#""try":"null""#,
        r#""zero":"timeout""#,
        r#""short":"timeout""#,
    ] {
        assert!(
            contexts[0].contains(fragment),
            "missing {fragment} in {:?}",
            contexts[0]
        );
    }
    Ok(())
}

/// A `writeBody()` with no prior `writeHead()` commits an implicit 200.
#[test]
fn implicit_200_on_first_write_body() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/")?;
    assert_eq!((status, body.as_str()), (200, "state=false"));
    Ok(())
}

/// A second finalizing verb after the unit sealed throws
/// `AlreadyFinalizedError`; the already-sealed response is untouched.
#[test]
fn double_finalize_throws_already_finalized() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/verbs-worker.php")))?;
    let h = r.handle()?;

    let (status, body) = drain(h.handle_blocking(req(
        "/?probe=double-finalize",
        "dispatcher/verbs-worker.php",
    ))?);
    assert_eq!((status, body.as_str()), (200, "first"));

    drop(h);
    r.shutdown();

    let records: Vec<String> = captured()
        .iter()
        .filter(|c| c.target == "app" && c.message == "double-finalize")
        .map(|c| c.context.clone())
        .collect();
    assert_eq!(records.len(), 1, "one throw record (got {records:?})");
    assert!(
        records[0].contains(r#""class":"Rapira\\Exception\\AlreadyFinalizedError""#),
        "wrong exception class: {:?}",
        records[0]
    );
    Ok(())
}

/// A second `writeHead()` after the final head throws
/// `HeadAlreadyWrittenError`; the first head stands.
#[test]
fn double_head_throws_head_already_written() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=double-head")?;
    assert_eq!(status, 201, "the first head must stand");
    assert_eq!(
        body,
        "double-head:Rapira\\Http\\Exception\\HeadAlreadyWrittenError"
    );
    Ok(())
}

/// Out-of-range status codes, non-token header names, CR/LF in values, and a
/// non-list entry all raise `\ValueError` before anything reaches the host.
#[test]
fn status_range_and_header_shape_value_errors() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=value-errors")?;
    assert_eq!(
        (status, body.as_str()),
        (200, "range:99;range:600;name;value;shape")
    );
    Ok(())
}

/// A 1xx head (other than 101) is advisory: accepted, dropped, and the unit
/// stays open for the real head.
#[test]
fn interim_head_is_dropped() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=interim")?;
    assert_eq!(
        (status, body.as_str()),
        (200, "after-interim finalized=false")
    );
    Ok(())
}

/// 101 is the carve-out in the 1xx interim rule: it commits as the final head
/// and locks out any later `writeHead()`.
#[test]
fn writehead_101_commits_as_final() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=upgrade")?;
    assert_eq!((status, body.as_str()), (101, "locked"));
    Ok(())
}

/// Buffered chunks concatenate, the unit stays unfinalized between them, and
/// the first chunk commits the implicit 200 — a later `writeHead()` must see
/// `HeadAlreadyWrittenError` instead of restamping the status.
#[test]
fn chunked_body_buffers_and_locks_the_head() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=chunks")?;
    assert_eq!((status, body.as_str()), (200, "one-mid=false"));

    let (status, body) = verbs_probe("/?probe=head-after-chunk")?;
    assert_eq!((status, body.as_str()), (200, "partial|locked"));
    Ok(())
}

/// Multi-value lists flatten to one field line per value, and PHP references
/// at both nesting levels (entry and item) are seen through.
#[test]
fn multi_value_and_reference_headers_flatten() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/verbs-worker.php")))?;
    let h = r.handle()?;

    let frame =
        drain_frame(h.handle_blocking(req("/?probe=multi", "dispatcher/verbs-worker.php"))?)
            .expect("unit must seal");
    let head = frame.head.as_ref().expect("head committed");
    assert_eq!(head.status, 200);
    let multi: Vec<String> = head
        .headers
        .iter()
        .filter(|(k, _)| k == "x-multi")
        .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
        .collect();
    assert_eq!(multi, ["a", "b"], "one field line per list value, in order");
    assert_eq!(header_value(&frame, "x-ref").as_deref(), Some("r1"));
    assert_eq!(header_value(&frame, "x-vref").as_deref(), Some("c1"));

    drop(h);
    r.shutdown();
    Ok(())
}

/// `receive()` while a unit is unfinalized throws the single-flight `\Error`
/// instead of deadlocking the worker on itself.
#[test]
fn receive_while_unfinalized_throws() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=busy")?;
    assert_eq!(status, 200);
    assert!(
        body.contains("busy:receive() while a Rapira\\Http\\Exchange is unfinalized"),
        "single-flight error must surface: {body:?}"
    );
    Ok(())
}

/// An Exchange dropped without finalizing fails that unit only: its Frame
/// sender dies unsent (the client sees the upstream failure), and the worker
/// serves the next unit normally.
#[test]
fn abandoned_exchange_fails_that_unit_only() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/verbs-worker.php")))?;
    let h = r.handle()?;

    let (status, body) =
        drain(h.handle_blocking(req("/?probe=abandon", "dispatcher/verbs-worker.php"))?);
    assert_eq!(
        (status, body.as_str()),
        (0, ""),
        "an abandoned unit must die unsent, not fake a response"
    );

    let (status, body) = drain(h.handle_blocking(req("/", "dispatcher/verbs-worker.php"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "state=false"),
        "the worker must keep serving after an abandoned unit"
    );

    drop(h);
    r.shutdown();
    Ok(())
}

/// `exit()` after serving must land as `Cycle::Recycle`: the script re-runs
/// and the worker keeps serving instead of shedding as a boot failure.
#[test]
fn exit_after_serving_recycles_the_worker() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/verbs-worker.php")))?;
    let h = r.handle()?;

    let (status, body) =
        drain(h.handle_blocking(req("/?probe=exit", "dispatcher/verbs-worker.php"))?);
    assert_eq!((status, body.as_str()), (200, "bye"));

    let (status, body) = drain(h.handle_blocking(req("/", "dispatcher/verbs-worker.php"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "state=false"),
        "the worker must re-run the script after exit()"
    );

    drop(h);
    r.shutdown();
    Ok(())
}

/// No body on a HEAD response or a 204 — chunks are accepted and dropped at
/// seal, the head stands.
/// https://www.rfc-editor.org/rfc/rfc9112#section-6.3
#[test]
fn head_and_204_drop_the_body() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/verbs-worker.php")))?;
    let h = r.handle()?;

    // The harness builder hardcodes GET; this test is about the method.
    let mut head_rq = req("/", "dispatcher/verbs-worker.php");
    head_rq.method = "HEAD".into();
    let (status, body) = drain(h.handle_blocking(head_rq)?);
    assert_eq!(
        (status, body.as_str()),
        (200, ""),
        "HEAD keeps the GET head but drops the body"
    );

    let (status, body) =
        drain(h.handle_blocking(req("/?probe=head204", "dispatcher/verbs-worker.php"))?);
    assert_eq!((status, body.as_str()), (204, ""));

    drop(h);
    r.shutdown();
    Ok(())
}

/// The `Rapira\Http\Request` field mapping: method, synthesized absolute
/// `$uri`, raw `$target`, Host-derived `$authority`, headers (including the
/// integer-normalized all-digit name), body, the `InetAddress` remote, null
/// `$tls`, and an intake-stamped `$receivedAt`.
#[test]
fn request_fields_reach_php() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/request-worker.php")))?;
    let h = r.handle()?;

    let mut rq = req("/path?x=1", "dispatcher/request-worker.php");
    rq.headers = vec![
        ("host".into(), b"example.test".to_vec()),
        ("x-probe".into(), b"alpha".to_vec()),
        ("123".into(), b"numeric".to_vec()),
    ];
    rq.body = Box::new(Cursor::new(b"hello".to_vec()));
    rq.content_length = 5;
    let (status, body) = drain(h.handle_blocking(rq)?);
    assert_eq!(status, 200);
    for line in [
        "method=GET",
        "uri=http://example.test/path?x=1",
        "target=/path?x=1",
        "authority='example.test'",
        "protocol=HTTP/1.1",
        "x-probe=alpha",
        "h123=numeric",
        "body=hello",
        "remote=Rapira\\InetAddress",
        "remote-ip=127.0.0.1",
        "remote-port=8080",
        "tls-null=true",
        "received-at-positive=true",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }

    drop(h);
    r.shutdown();
    Ok(())
}

/// `getInfo()` while handling: the outstanding unit counts as active. The
/// counts are deterministic here — the one job was decremented from `pending`
/// at pull, and nothing else is queued — so assert them exactly (a `>= 0`
/// assertion would pass for almost any broken counter).
#[test]
fn get_info_counts_the_outstanding_unit() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=info")?;
    assert_eq!((status, body.as_str()), (200, "pending=0 active=1"));
    Ok(())
}
