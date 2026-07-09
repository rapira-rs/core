//! Host import: `exec` runs an request through rapira's PHP pool and returns
//! the response. It's an async, store-accessing import (the guest may `join!`
//! several concurrently), so it clones the `RapiraHandle` out of the store and
//! then awaits without holding any store borrow.

use crate::state::HostState;
use crate::wit::rapira::extension::host::HostWithStore;
// Aliased: the WIT `request`/`response` types collide with `php_sys::Request`.
use crate::wit::rapira::extension::types::{Request as ExtRequest, Response as ExtResponse};
use php_sys::{Frame, RapiraHandle, Request};
use std::io::Cursor;
use std::path::PathBuf;
use tokio::sync::mpsc::Receiver;
use wasmtime::component::{Accessor, HasSelf};

// The store-data type must satisfy the (function-less) `Host` traits; `exec` lives
// in `HostWithStore` (it needs the store to reach the RapiraHandle).
impl crate::wit::rapira::extension::types::Host for HostState {}
impl crate::wit::rapira::extension::host::Host for HostState {}

impl HostWithStore<HostState> for HasSelf<HostState> {
    async fn exec(
        accessor: &Accessor<HostState, Self>,
        req: ExtRequest,
    ) -> Result<ExtResponse, String> {
        // Clone the handle out of the store, then await without holding the store —
        // so concurrent execs don't contend on it.
        let rapira = accessor.with(|mut access| access.get().rapira.clone());
        run(&rapira, req).await
    }
}

async fn run(rapira: &RapiraHandle, req: ExtRequest) -> Result<ExtResponse, String> {
    let request: Request = to_request(req);
    let mut rx: Receiver<Frame> = rapira.handle(request).await.map_err(|e| e.to_string())?;

    let mut response: ExtResponse = ExtResponse {
        status: 0,
        headers: Vec::new(),
        body: Vec::new(),
    };
    while let Some(frame) = rx.recv().await {
        match frame {
            Frame::Head(head) => {
                response.status = head.status;
                // Pass header-value bytes through unchanged (WIT value type is list<u8>);
                // a lossy String decode here would corrupt latin1/binary header values.
                response.headers = head.headers;
            }
            Frame::Body(bytes) => response.body.extend_from_slice(&bytes),
        }
    }
    Ok(response)
}

/// Build a rapira `Request` from the guest's request. In Worker mode the
/// resident script handles it; the file-ish fields are `$_SERVER` metadata only.
fn to_request(req: ExtRequest) -> Request {
    let (path, query) = req.uri.split_once('?').unwrap_or((&req.uri, ""));
    let content_type = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.clone());

    Request {
        method: req.method,
        https: false,
        query: query.to_string(),
        protocol: "HTTP/1.1".to_string(),
        remote_addr: "127.0.0.1".to_string(),
        server_name: "localhost".to_string(),
        server_port: "80".to_string(),
        remote_port: "0".to_string(),
        script_name: path.to_string(),
        document_root: String::new(),
        script_filename: PathBuf::from(path),
        content_type,
        content_length: req.body.len() as i64,
        body: Box::new(Cursor::new(req.body)),
        headers: req.headers,
        server_vars: Vec::new(),
        uri: req.uri,
    }
}
