//! Integration-test helpers + PHP fixtures for the rapira/core workspace.
//!
//! Each integration-test FILE under `tests/` becomes its own test binary, i.e. its own
//! OS process — which matters here: PHP's embed SAPI global init (`php_tsrm_startup_ex` /
//! `sapi_startup` / `php_module_startup`) is a process-global singleton, so a process may
//! call `Rapira::boot()` **exactly once**. Keep one booting test per file.

use php_sys::{Frame, Request};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

/// Absolute path to a PHP fixture shipped with this crate (robust to the test's cwd).
pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

/// Build a minimal `GET` request for `uri`, with `$_SERVER` metadata pointing at `fixture_name`.
pub fn req(uri: &str, fixture_name: &str) -> Request {
    let query = uri.splitn(2, '?').nth(1).unwrap_or("").to_string();
    Request {
        method: "GET".into(),
        uri: uri.into(),
        query,
        protocol: "HTTP/1.1".into(),
        remote_addr: "127.0.0.1".into(),
        server_name: "localhost".into(),
        server_port: "8080".into(),
        script_filename: fixture(fixture_name),
        script_name: "/index.php".into(),
        headers: vec![],
        server_vars: vec![],
        content_type: None,
        content_length: 0,
        body: Box::new(std::io::empty()),
    }
}

/// Drain a response stream to `(status, body)`. Status is 0 if no head frame arrived.
pub fn drain(mut rx: mpsc::Receiver<Frame>) -> (u16, String) {
    let (mut status, mut body) = (0u16, Vec::new());
    while let Some(frame) = rx.blocking_recv() {
        match frame {
            Frame::Head(h) => status = h.status,
            Frame::Body(b) => body.extend_from_slice(&b),
        }
    }
    (status, String::from_utf8_lossy(&body).into_owned())
}
