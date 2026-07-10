//! Native rapira extension contract.
//!
//! An extension is an in-repo crate that **drives PHP**: its async [`Extension::run`]
//! reaches rapira's PHP worker pool through [`Php`]. The host constructs it
//! ([`Extension::init`]), drives `run`, and asks it to stop with [`Extension::shutdown`].

use php_sys::{RapiraHandle, Request as PhpRequest};
use std::future::Future;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;

/// Fallible SDK paths report `anyhow::Error`; the host renders it to a log line.
pub type Result<T = (), E = anyhow::Error> = std::result::Result<T, E>;

/// Re-exported so streaming extensions can match on frames from [`Php::stream`].
pub use php_sys::Frame;

/// A native rapira extension: a long-lived service that drives PHP via [`Php`].
///
/// Lifecycle: `init` (construct) → `run` (serve) → `shutdown` (drain). `run` and
/// `shutdown` are never borrowed at once — the host drops the in-flight `run` future
/// before it calls `shutdown` (see `extension_host`).
pub trait Extension: Send + 'static {
    /// Construct the extension. Cheap and infallible; heavy setup belongs in `run`.
    fn init() -> Self
    where
        Self: Sized;

    /// Stable id for logs; unique across the registry.
    fn name(&self) -> &str;

    /// Drive to completion. Serve requests here, reaching PHP through `php`. `Ok` on a
    /// clean finish, `Err` to report a failure. Must stay cooperative — reach `.await`
    /// points regularly so the host can cancel `run` on shutdown; a tight non-awaiting
    /// loop cannot be stopped.
    ///
    /// This parameter is the SDK's stability line: future capabilities land as methods
    /// on [`Php`], never as new `run` parameters.
    fn run(&mut self, php: Php) -> impl Future<Output = Result<()>> + Send;

    /// Stop gracefully (drain in-flight work, release the socket). The host calls this
    /// once, after cancelling `run`, and bounds it with its own timeout.
    fn shutdown(&mut self) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
}

/// The PHP bridge handed to every extension. Cheap to clone; each clone holds one
/// `RapiraHandle` — never keep a spare past the owning `Rapira` (shutdown contract).
#[derive(Clone)]
pub struct Php {
    rapira: RapiraHandle,
    script: Arc<ScriptMeta>,
}

/// The configured PHP entry script plus the CGI vars derived from it, computed once
/// at construction instead of per request.
struct ScriptMeta {
    /// → SCRIPT_FILENAME
    filename: PathBuf,
    /// → DOCUMENT_ROOT (the script's parent directory)
    document_root: String,
    /// → SCRIPT_NAME, e.g. "/index.php"
    script_name: String,
}

impl ScriptMeta {
    fn new(filename: PathBuf) -> Self {
        let document_root = filename
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let script_name = filename
            .file_name()
            .map_or_else(|| "/".to_string(), |f| format!("/{}", f.to_string_lossy()));
        Self {
            filename,
            document_root,
            script_name,
        }
    }
}

impl Php {
    /// Host-internal: `extension_host` builds one and clones it into every `run`.
    /// Not part of the extension-facing API and not semver-guarded.
    #[doc(hidden)]
    pub fn new(rapira: RapiraHandle, script: PathBuf) -> Self {
        Self {
            rapira,
            script: Arc::new(ScriptMeta::new(script)),
        }
    }

    /// The entry script every request runs (front controller / worker).
    pub fn script(&self) -> &Path {
        &self.script.filename
    }

    /// Submit `req` and stream PHP response frames as produced — for proxying to a
    /// client without buffering the body. A well-formed stream ends with
    /// [`Frame::End`]; a channel that closes without one means the worker died
    /// mid-response.
    pub async fn stream(&self, req: Request) -> Result<Receiver<Frame>> {
        self.rapira.handle(to_request(&self.script, req)).await
    }

    /// Submit `req` and collect the whole response (buffers the body). Errors when
    /// PHP produced no response head, when the worker died mid-response (the stream
    /// ends without its [`Frame::End`] marker), or when PHP errored after it began
    /// streaming its body (so the buffered body may be incomplete).
    pub async fn exec(&self, req: Request) -> Result<Response> {
        let mut rx: Receiver<Frame> = self.stream(req).await?;
        let mut resp: Response = Response::default();
        let mut head_seen = false;
        let mut end: Option<bool> = None;
        while let Some(frame) = rx.recv().await {
            match frame {
                // Header-value bytes pass through unchanged: PHP may emit latin1/binary.
                Frame::Head(head) => {
                    head_seen = true;
                    resp.status = head.status;
                    resp.headers = head.headers;
                }
                Frame::Body(bytes) => resp.body.extend_from_slice(&bytes),
                Frame::End { truncated } => end = Some(truncated),
            }
        }
        match end {
            Some(false) if head_seen => Ok(resp),
            Some(false) => Err(anyhow::anyhow!("php produced no response head")),
            Some(true) => Err(anyhow::anyhow!("php crashed mid-response; body truncated")),
            None => Err(anyhow::anyhow!(
                "php worker died mid-response (stream ended without its end marker)"
            )),
        }
    }
}

/// A request an extension runs through PHP. php_sys-internal fields (`query`,
/// `content_type`, script paths) are derived by `to_request`.
pub struct Request {
    pub method: String,
    pub uri: String, // path + optional ?query → REQUEST_URI
    pub https: bool,
    pub protocol: String, // "HTTP/1.1"
    pub remote_addr: String,
    pub remote_port: u16,
    pub server_name: String,
    pub server_port: u16,
    /// Header values are raw bytes (latin1/binary-safe), mirroring [`Response`]:
    /// a client may send octets that are not valid UTF-8 and PHP must see them verbatim.
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
}

impl Request {
    /// A bodyless `GET` for `uri` with sensible defaults (tests / internal calls).
    pub fn get(uri: &str) -> Self {
        Self {
            method: "GET".into(),
            uri: uri.into(),
            https: false,
            protocol: "HTTP/1.1".into(),
            remote_addr: "127.0.0.1".into(),
            remote_port: 0,
            server_name: "localhost".into(),
            server_port: 80,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>, // bytes: latin1/binary-safe
    pub body: Vec<u8>,
}

/// The one place the `Request → php_sys::Request` mapping lives.
fn to_request(script: &ScriptMeta, req: Request) -> PhpRequest {
    let query = req.uri.split_once('?').map_or("", |(_, q)| q).to_string();
    let content_type = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| String::from_utf8_lossy(v).into_owned());

    PhpRequest {
        method: req.method,
        https: req.https,
        query,
        protocol: req.protocol,
        remote_addr: req.remote_addr,
        server_name: req.server_name,
        server_port: req.server_port.to_string(),
        remote_port: req.remote_port.to_string(),
        script_name: script.script_name.clone(),
        document_root: script.document_root.clone(),
        script_filename: script.filename.clone(),
        content_type,
        content_length: req.body.len() as i64,
        body: Box::new(Cursor::new(req.body)),
        headers: req.headers,
        server_vars: Vec::new(),
        uri: req.uri,
    }
}
