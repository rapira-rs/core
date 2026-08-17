//! Streaming over the wire: the frame protocol as an HTTP client sees it -
//! flush timing, chunked framing, keepalive reuse, interim heads, the CLEE
//! prefix, client aborts, and truncated closes.

use std::time::{Duration, Instant};

use crate::harness::{Conn, decode_chunked, http_get_raw, spawn_with_config, wait_log_contains};

const T: Duration = Duration::from_secs(10);

/// `flush()` puts the head on the wire while the first body byte is still
/// 400ms away; each later chunk arrives while the response is open.
#[test]
fn sse_head_and_events_reach_the_wire_incrementally() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=sse HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("send");

    let started = Instant::now();
    let (status, fields) = c.read_head(T).expect("flushed head");
    assert_eq!(status, 200);
    assert!(
        started.elapsed() < Duration::from_millis(350),
        "the head must beat the 400ms-late first event (took {:?})",
        started.elapsed()
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "transfer-encoding" && v == "chunked"),
        "flush costs the length: chunked framing expected, got {fields:?}"
    );

    c.read_body_until(b"data: one", T).expect("first event");
    c.read_body_until(b"data: two", T).expect("second event");
    let rest = c.read_remaining(T).expect("clean end");
    assert!(
        String::from_utf8_lossy(&rest).ends_with("0\r\n\r\n"),
        "the chunked terminator must close the stream"
    );
}

/// A chunked stream keeps the connection reusable: a second request on the
/// same socket is served.
#[test]
fn chunked_stream_preserves_keepalive() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let mut c = Conn::open(srv.addr, T).expect("connect");

    c.send(b"GET /?probe=chunks HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");
    let (status, fields) = c.read_head(T).expect("head");
    assert_eq!(status, 200);
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "transfer-encoding" && v == "chunked"),
        "{fields:?}"
    );
    c.read_body_until(b"0\r\n\r\n", T).expect("terminator");

    c.send(b"GET /?probe=chunks HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("second request on the same connection");
    let (status, _) = c.read_head(T).expect("reused connection must serve");
    assert_eq!(status, 200);
}

/// A 103 goes out as its own head block ahead of the final 200.
#[test]
fn interim_head_precedes_the_final_head() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=interim HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("send");

    let (status, fields) = c.read_head(T).expect("interim head");
    assert_eq!(status, 103);
    assert!(fields.iter().any(|(k, _)| k == "link"), "{fields:?}");
    let (status, _) = c.read_head(T).expect("final head");
    assert_eq!(status, 200);
    c.read_body_until(b"hello", T).expect("body");
}

/// An HTTP/1.0 client gets neither the interim head nor chunked framing.
#[test]
fn http10_gets_no_interim_and_no_chunked() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");

    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=interim HTTP/1.0\r\n\r\n")
        .expect("send");
    let (status, fields) = c.read_head(T).expect("head");
    assert_eq!(status, 200, "the interim head must be dropped for 1.0");
    assert!(
        !fields.iter().any(|(k, _)| k == "transfer-encoding"),
        "no chunked toward a 1.0 client: {fields:?}"
    );
    let rest = c.read_remaining(T).expect("close-delimited body");
    assert_eq!(rest, b"hello");
}

/// The CLEE prefix on the wire: content-length 5, exactly the 5 fitting bytes,
/// a clean end (keepalive-safe framing).
#[test]
fn content_length_exceeded_serves_the_declared_prefix() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let raw = http_get_raw(srv.addr, "/?probe=cl-exceeded", &[], T).expect("response");
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.to_ascii_lowercase().contains("content-length: 5"),
        "declared length must be honoured: {text}"
    );
    // anchored to the head terminator: the body is exactly the 5 bytes
    assert!(
        text.ends_with("\r\n\r\n01234"),
        "exactly the fitting prefix: {text}"
    );
}

/// A client that walks away mid-stream surfaces as WorkDiscardedException in
/// the worker (visible through the app log).
#[test]
fn client_abort_discards_the_unit() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=discard HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");
    // the flushed head proves the unit is out with PHP - leaving earlier can
    // race the pre-handout probe, which fails the unit before PHP sees it
    let (status, _) = c.read_head(T).expect("flushed head");
    assert_eq!(status, 200);
    c.abandon(); // leave while the worker sleeps 300ms

    assert!(
        wait_log_contains(&srv, "WorkDiscardedException", T),
        "the worker must observe the abort on its next write"
    );
}

/// A worker dying mid-stream must not fake a clean end: the chunked terminator
/// never arrives.
#[test]
fn worker_death_mid_stream_truncates_the_response() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=die-mid-stream HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");
    let (status, _) = c.read_head(T).expect("head");
    assert_eq!(status, 200);
    c.read_body_until(b"first,", T).expect("first chunk");
    let rest = c.read_remaining(T).expect("connection drops");
    let full = [b"6\r\nfirst,\r\n".to_vec(), rest].concat();
    assert!(
        decode_chunked(&full).is_err(),
        "no clean terminator after a mid-stream death"
    );
}

/// sendFile: the host streams the file with a real content-length; PHP never
/// holds the bytes.
#[test]
fn sendfile_streams_from_disk_with_length() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    // under the default root (the entrypoint's directory = the scratch dir)
    let payload = srv.dir.join("payload.bin");
    std::fs::write(&payload, vec![b'z'; 100_000]).expect("payload");

    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(
        format!(
            "GET /?probe=sendfile HTTP/1.1\r\nHost: e2e\r\nx-path: {}\r\nConnection: close\r\n\r\n",
            payload.display()
        )
        .as_bytes(),
    )
    .expect("send");
    let (status, fields) = c.read_head(T).expect("head");
    assert_eq!(status, 200);
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "content-length" && v == "100000"),
        "the file length is known up front: {fields:?}"
    );
    let rest = c.read_remaining(T).expect("body");
    assert_eq!(rest.len(), 100_000);
    assert!(rest.iter().all(|&b| b == b'z'), "file bytes intact");
}

/// Trailers are dropped on h1 - the response still ends cleanly, with no
/// trailer bytes in the chunked epilogue, and the connection stays reusable.
#[test]
fn trailers_are_dropped_on_h1_and_the_response_ends_cleanly() {
    let srv = spawn_with_config("lifecycle/stream-worker.php", 1, "");
    let mut c = Conn::open(srv.addr, T).expect("connect");
    c.send(b"GET /?probe=trailers HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("send");

    let (status, fields) = c.read_head(T).expect("head");
    assert_eq!(status, 200);
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "transfer-encoding" && v == "chunked"),
        "{fields:?}"
    );
    c.read_body_until(b"payload", T).expect("body");
    c.read_body_until(b"0\r\n\r\n", T)
        .expect("a clean terminator with no trailer section");

    // the connection survives the dropped section
    c.send(b"GET /?probe=chunks HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("second request");
    let (status, _) = c.read_head(T).expect("reused connection must serve");
    assert_eq!(status, 200);
}
