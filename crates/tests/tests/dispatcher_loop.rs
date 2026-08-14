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
    rq2.body = php_sys::types::Body::Raw(Box::new(Cursor::new(b"two".to_vec())));
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

/// Out-of-range status codes, non-token header names, CR/LF in values, a
/// non-list entry, an integer array key, and a non-string list item all raise
/// `\ValueError` before anything reaches the host.
#[test]
fn status_range_and_header_shape_value_errors() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=value-errors")?;
    assert_eq!(
        (status, body.as_str()),
        (200, "range:99;range:600;name;value;shape;intkey;item")
    );
    Ok(())
}

/// Contract: an empty chunk without eos does nothing — in particular it must
/// not commit the implicit 200 and lock out a later `writeHead()`.
#[test]
fn empty_non_eos_chunk_does_nothing() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=empty-chunk")?;
    assert_eq!((status, body.as_str()), (404, "body"));
    Ok(())
}

/// The verb edges: `tryReceive()` while a unit is out is the single-flight
/// `\Error`, a timeout below -1 is a `\ValueError`, and `writeHead()` after
/// `eos: true` is `HeadAlreadyWrittenError` (writeHead's @throws set has no
/// AlreadyFinalizedError).
#[test]
fn verb_edges_throw_their_documented_classes() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/verbs-worker.php")))?;
    let h = r.handle()?;
    let (status, body) =
        drain(h.handle_blocking(req("/?probe=verb-edges", "dispatcher/verbs-worker.php"))?);
    assert_eq!((status, body.as_str()), (200, "try-busy;neg-timeout"));
    drop(h);
    r.shutdown();

    let records: Vec<String> = captured()
        .iter()
        .filter(|c| c.target == "app" && c.message == "head-after-eos")
        .map(|c| c.context.clone())
        .collect();
    assert_eq!(records.len(), 1, "one throw record (got {records:?})");
    assert!(
        records[0].contains(r#""class":"Rapira\\Http\\Exception\\HeadAlreadyWrittenError""#),
        "wrong exception class: {:?}",
        records[0]
    );
    Ok(())
}

/// The polling verbs' success paths: a unit comes out of `tryReceive()`, and
/// out of `receive(1s)` after the fixture flips modes.
#[test]
fn try_and_timed_receive_serve_units() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/poll-worker.php")))?;
    let h = r.handle()?;

    let (status, body) = drain(h.handle_blocking(req("/one", "dispatcher/poll-worker.php"))?);
    assert_eq!((status, body.as_str()), (200, "served-by=try target=/one"));

    let (status, body) =
        drain(h.handle_blocking(req("/two?mode=timed", "dispatcher/poll-worker.php"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "served-by=try target=/two?mode=timed")
    );

    let (status, body) = drain(h.handle_blocking(req("/three", "dispatcher/poll-worker.php"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "served-by=timed target=/three")
    );

    drop(h);
    r.shutdown();
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

/// An abandoned unit holding a host-parsed multipart body: the SpooledFile
/// drop net must unlink the spool the moment the exchange dies.
#[test]
fn abandoned_multipart_unit_unlinks_its_spool() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/verbs-worker.php")))?;
    let h = r.handle()?;

    let spool = std::env::temp_dir().join(format!("rapira-test-abandon-{}", std::process::id()));
    std::fs::write(&spool, b"PAYLOAD")?;
    let mut rq = req("/?probe=abandon", "dispatcher/verbs-worker.php");
    rq.body = php_sys::types::Body::Multipart(php_sys::types::MultipartBody {
        fields: vec![],
        files: vec![php_sys::types::UploadedFile {
            name: b"f".to_vec(),
            client_filename: b"a.bin".to_vec(),
            client_media_type: None,
            headers: vec![],
            file: php_sys::types::SpooledFile {
                path: spool.clone(),
            },
            size: 7,
        }],
    });
    let (status, body) = drain(h.handle_blocking(rq)?);
    assert_eq!((status, body.as_str()), (0, ""), "the unit dies unsent");
    assert!(
        !spool.exists(),
        "dropping the exchange must unlink the spooled file"
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

/// The `Rapira\Http\Request` field mapping: per-line headers (repeats stay a
/// list, casings stay distinct keys, all-digit names land as int keys),
/// byte-exact `$target` (non-UTF-8 byte included), plugin-supplied
/// `$authority`, the synthesized `$uri`, socket-typed addresses, null `$tls`,
/// and the intake fallback stamp for a stampless producer.
#[test]
fn request_fields_reach_php() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/request-worker.php")))?;
    let h = r.handle()?;

    let mut rq = req("/path?x=1", "dispatcher/request-worker.php");
    rq.headers = vec![
        ("x-probe".into(), b"alpha".to_vec()),
        ("X-Case".into(), b"one".to_vec()),
        ("x-probe".into(), b"beta".to_vec()),
        ("x-case".into(), b"two".to_vec()),
        ("123".into(), b"numeric".to_vec()),
        ("a".into(), b"solo".to_vec()),
        ("-".into(), b"dash".to_vec()),
        ("-1".into(), b"neg".to_vec()),
    ];
    rq.authority = Some(b"example.test".to_vec());
    // %2F survives un-decoded and the 0xE9 byte survives un-lossied
    rq.target = Some(b"/path%2Fa?x=1\xe9".to_vec());
    rq.body = php_sys::types::Body::Raw(Box::new(Cursor::new(b"hello".to_vec())));
    rq.content_length = 5;
    let (status, body) = drain(h.handle_blocking(rq)?);
    assert_eq!(status, 200);
    let target_hex = format!(
        "target-hex={}",
        b"/path%2Fa?x=1\xe9"
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    assert!(
        body.contains(&target_hex),
        "missing {target_hex:?} in {body:?}"
    );
    for line in [
        "method=GET",
        "uri=http://example.test/path?x=1",
        "authority='example.test'",
        "protocol=HTTP/1.1",
        "x-probe=alpha|beta",
        "x-case-keys=X-Case|x-case",
        "h123=numeric",
        "h-single=solo",
        "h-dash=dash",
        "h-neg=neg",
        "memo-same=true",
        "body=hello",
        "remote=Rapira\\InetAddress",
        "remote-detail=127.0.0.1:8080",
        "server=Rapira\\InetAddress",
        "server-detail=127.0.0.1:8080",
        "tls=NULL",
        "received-at-positive=true",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }

    // a second unit must serve cleanly after the first is freed (memo included)
    let (status, body) = drain(h.handle_blocking(req("/again", "dispatcher/request-worker.php"))?);
    assert_eq!(status, 200, "body: {body:?}");
    assert!(
        body.contains("memo-same=true"),
        "fresh memo on the new unit"
    );

    drop(h);
    r.shutdown();
    Ok(())
}

/// Producer-stamped facts pass through untouched: an exact `receivedAt`, the
/// contract protocol spelling (HTTP/2.0 → HTTP/2), and the no-authority `$uri`
/// fallback to the server socket.
#[test]
fn plugin_stamped_fields_pass_through() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/request-worker.php")))?;
    let h = r.handle()?;

    let mut rq = req("/p", "dispatcher/request-worker.php");
    rq.protocol = "HTTP/2.0".into();
    rq.received_at = Some(123.5);
    let (status, body) = drain(h.handle_blocking(rq)?);
    assert_eq!(status, 200);
    for line in [
        "protocol=HTTP/2",
        "received-at=123.5",
        "authority=NULL",
        "uri=http://127.0.0.1:8080/p",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }

    drop(h);
    r.shutdown();
    Ok(())
}

/// The UnixAddress arms: an unnamed peer and a path-carrying listener.
#[test]
fn unix_address_arms_reach_php() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/request-worker.php")))?;
    let h = r.handle()?;

    let mut rq = req("/", "dispatcher/request-worker.php");
    rq.remote = php_sys::types::Addr::Unix(None);
    rq.server = php_sys::types::Addr::Unix(Some("/run/rapira.sock".into()));
    let (status, body) = drain(h.handle_blocking(rq)?);
    assert_eq!(status, 200);
    for line in [
        "remote=Rapira\\UnixAddress",
        "remote-detail=NULL",
        "server=Rapira\\UnixAddress",
        "server-detail='/run/rapira.sock'",
        // a unix listener has no host:port of its own: $uri falls back to the
        // configured name
        "uri=http://localhost:8080/",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }

    drop(h);
    r.shutdown();
    Ok(())
}

/// The remaining `$uri` synthesis arms: the https scheme, and an asterisk-form
/// target collapsing to the authority root; HTTP/3.0 maps to the contract
/// spelling on the way.
#[test]
fn uri_synthesis_covers_https_and_asterisk_form() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/request-worker.php")))?;
    let h = r.handle()?;

    let mut rq = req("/secure", "dispatcher/request-worker.php");
    rq.https = true;
    rq.protocol = "HTTP/3.0".into();
    let (status, body) = drain(h.handle_blocking(rq)?);
    assert_eq!(status, 200);
    for line in ["uri=https://127.0.0.1:8080/secure", "protocol=HTTP/3"] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }

    let mut rq = req("*", "dispatcher/request-worker.php");
    rq.method = "OPTIONS".into();
    let (status, body) = drain(h.handle_blocking(rq)?);
    assert_eq!(status, 200);
    for line in [
        "method=OPTIONS",
        "uri=http://127.0.0.1:8080/",
        "target-hex=2a",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }

    drop(h);
    r.shutdown();
    Ok(())
}

/// The Tls plumbing: full object with a client cert, then the nullable arms.
#[test]
fn tls_view_reaches_php() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/request-worker.php")))?;
    let h = r.handle()?;

    let mut rq = req("/", "dispatcher/request-worker.php");
    rq.tls = Some(php_sys::types::TlsView {
        version: "TLSv1.3".into(),
        cipher: "TLS_AES_256_GCM_SHA384".into(),
        alpn: Some("h2".into()),
        server_name: Some("sni.example".into()),
        cert: Some(php_sys::types::ClientCertView {
            serial: "0AB1".into(),
            organization: None,
            fingerprint: "abcd".into(),
        }),
    });
    let (status, body) = drain(h.handle_blocking(rq)?);
    assert_eq!(status, 200);
    assert!(
        body.contains("tls=TLSv1.3|TLS_AES_256_GCM_SHA384|'h2'|'sni.example'|'0AB1'|NULL|'abcd'"),
        "unexpected tls line in {body:?}"
    );

    let mut rq = req("/", "dispatcher/request-worker.php");
    rq.tls = Some(php_sys::types::TlsView {
        version: "TLSv1.2".into(),
        cipher: "X".into(),
        alpn: None,
        server_name: None,
        cert: None,
    });
    let (status, body) = drain(h.handle_blocking(rq)?);
    assert_eq!(status, 200);
    assert!(
        body.contains("tls=TLSv1.2|X|NULL|NULL|NULL|NULL|NULL"),
        "unexpected tls line in {body:?}"
    );

    drop(h);
    r.shutdown();
    Ok(())
}

/// A host-parsed Multipart reaches PHP as the object graph, and the spool file
/// is gone the moment the response frame arrives: seal() unlinks before the
/// frame is sent.
#[test]
fn multipart_body_reaches_php_and_spools_die_at_seal() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/multipart-worker.php")))?;
    let h = r.handle()?;

    let spool = std::env::temp_dir().join(format!("rapira-test-mp-{}", std::process::id()));
    std::fs::write(&spool, b"PAYLOAD")?;

    let mut rq = req("/", "dispatcher/multipart-worker.php");
    rq.body = php_sys::types::Body::Multipart(php_sys::types::MultipartBody {
        fields: vec![php_sys::types::FormField {
            name: b"note".to_vec(),
            value: b"hello".to_vec(),
            headers: vec![(
                "content-disposition".into(),
                b"form-data; name=\"note\"".to_vec(),
            )],
        }],
        files: vec![php_sys::types::UploadedFile {
            name: b"f".to_vec(),
            client_filename: b"a.bin".to_vec(),
            client_media_type: Some(b"application/octet-stream".to_vec()),
            headers: vec![(
                "content-disposition".into(),
                b"form-data; name=\"f\"; filename=\"a.bin\"".to_vec(),
            )],
            file: php_sys::types::SpooledFile {
                path: spool.clone(),
            },
            size: 7,
        }],
    });
    let (status, body) = drain(h.handle_blocking(rq)?);
    assert_eq!(status, 200, "body: {body:?}");
    for line in [
        "class=Rapira\\Http\\Multipart",
        "counts=1/1",
        "field0=note=hello",
        "field0-cd=true",
        "file0=f:a.bin:7:PAYLOAD",
        "file0-type='application/octet-stream'",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    assert!(
        !spool.exists(),
        "seal() must unlink the spool before the frame goes out"
    );

    drop(h);
    r.shutdown();
    Ok(())
}

/// Two fields and two files with distinct contents: the graph must pair each
/// part with its own headers, spool path, and size by index.
#[test]
fn multipart_parts_stay_index_aligned() -> anyhow::Result<()> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Dispatcher(fixture("dispatcher/multipart-worker.php")))?;
    let h = r.handle()?;

    let pid = std::process::id();
    let spool_a = std::env::temp_dir().join(format!("rapira-test-mpa-{pid}"));
    let spool_b = std::env::temp_dir().join(format!("rapira-test-mpb-{pid}"));
    std::fs::write(&spool_a, b"AAA")?;
    std::fs::write(&spool_b, b"BBBBB")?;

    let file = |name: &[u8], filename: &[u8], path: &std::path::Path, size: u64| {
        php_sys::types::UploadedFile {
            name: name.to_vec(),
            client_filename: filename.to_vec(),
            client_media_type: None,
            headers: vec![],
            file: php_sys::types::SpooledFile {
                path: path.to_path_buf(),
            },
            size,
        }
    };
    let field = |name: &[u8], value: &[u8]| php_sys::types::FormField {
        name: name.to_vec(),
        value: value.to_vec(),
        headers: vec![("content-disposition".into(), b"form-data".to_vec())],
    };
    let mut rq = req("/", "dispatcher/multipart-worker.php");
    rq.body = php_sys::types::Body::Multipart(php_sys::types::MultipartBody {
        fields: vec![field(b"one", b"1"), field(b"two", b"22")],
        files: vec![
            file(b"fa", b"a.bin", &spool_a, 3),
            file(b"fb", b"b.bin", &spool_b, 5),
        ],
    });
    let (status, body) = drain(h.handle_blocking(rq)?);
    assert_eq!(status, 200, "body: {body:?}");
    for line in [
        "counts=2/2",
        "field0=one=1",
        "field1=two=22",
        "file0=fa:a.bin:3:AAA",
        "file1=fb:b.bin:5:BBBBB",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    assert!(!spool_a.exists() && !spool_b.exists(), "seal unlinks both");

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
