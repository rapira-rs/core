//! Native rapira extension contract.
//!
//! An extension is a standalone crate — its own repository, compiled into rapira — that
//! **drives PHP**: its async [`Extension::run`] reaches rapira's PHP worker pool through
//! [`Php`]. The host constructs it ([`Extension::init`], injecting its typed
//! [`Extension::Config`]), drives `run`, and asks it to stop with
//! [`Extension::shutdown`].

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

mod prepare;
pub use prepare::{LISTEN_BACKLOG, ListenAddr, PrepareCtx, PreparedListener};

/// Fallible SDK paths report `anyhow::Error`; the host renders it to a log line.
pub type Result<T = (), E = anyhow::Error> = std::result::Result<T, E>;

/// A native rapira extension: a long-lived service that drives PHP via [`Php`].
///
/// Lifecycle: `init` (construct, injecting [`Extension::Config`]) → `prepare`
/// (master-side, pre-fork: bind inheritable resources) → `run` (serve, in the worker
/// process) → `shutdown` (drain). `run` and `shutdown` are never borrowed at once — the
/// host drops the in-flight `run` future before it calls `shutdown` (see
/// `rapira_runtime`).
pub trait Extension: Send + 'static {
    /// Extension-specific configuration, injected at construction. `()` when the
    /// extension needs none.
    type Config;

    /// Construct the extension: internal initialization plus storing `config`. Cheap
    /// and infallible; heavy setup belongs in `run`.
    fn init(config: Self::Config) -> Self
    where
        Self: Sized;

    /// Stable id for logs; unique across the registry.
    fn name(&self) -> &str;

    /// Master-side pre-fork hook: synchronous, single-threaded, no runtime exists.
    /// Runs once after `init`, before any fork and before `run`. Bind inheritable
    /// resources here (listen sockets via [`PrepareCtx`]) and store them in `self` —
    /// this same value crosses the fork, and `run` consumes them in the worker.
    /// Must not spawn threads or create runtime primitives. Default: no-op
    /// (queue-consumer extensions prepare nothing).
    fn prepare(&mut self, _ctx: &mut PrepareCtx) -> Result<()> {
        Ok(())
    }

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

/// The host-side PHP executor behind [`Php`]. `rapira_runtime` implements it over the
/// worker pool; extensions only ever see [`Php`]. Host-internal: not part of the
/// extension-facing API and not semver-guarded.
#[doc(hidden)]
pub trait Backend: Send + Sync + 'static {
    /// Submit `req` and resolve with the whole response; the error contract lives on
    /// [`Php::exec`].
    fn exec(&self, req: Request) -> Pin<Box<dyn Future<Output = Result<Response>> + Send + '_>>;
}

/// The PHP bridge handed to every extension. Cheap to clone; every clone shares the
/// host's backend handle — never keep a spare past `run`/`shutdown` (the host's
/// shutdown contract).
#[derive(Clone)]
pub struct Php {
    backend: Arc<dyn Backend>,
    script: Arc<Path>,
}

impl Php {
    /// Host-internal: `rapira_runtime` builds one and clones it into every `run`.
    /// Not part of the extension-facing API and not semver-guarded.
    #[doc(hidden)]
    pub fn new(backend: Arc<dyn Backend>, script: PathBuf) -> Self {
        Self {
            backend,
            script: Arc::from(script),
        }
    }

    /// The entry script every request runs (front controller / worker).
    pub fn script(&self) -> &Path {
        &self.script
    }

    /// Submit `req` and collect the whole response — the worker seals it into a
    /// single frame, so the caller wakes once per response. Errors when PHP
    /// produced no response head, when the worker died mid-response (the channel
    /// closed without a frame), when PHP errored after it began writing its
    /// body (so the body may be incomplete), or with a downcastable [`Rejected`]
    /// when the host refused the body before dispatch (malformed multipart → 400,
    /// past a configured limit → 413) — nothing rejected ever reaches a worker.
    pub async fn exec(&self, req: Request) -> Result<Response> {
        self.backend.exec(req).await
    }
}

/// One endpoint of an accepted connection, as the socket reports it. The union mirrors
/// the contract's `InetAddress|UnixAddress`: a port exists exactly when the endpoint is
/// an IP one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addr {
    Inet(std::net::SocketAddr),
    /// None is an unnamed endpoint — the usual case for a peer connecting to a unix
    /// listener, which binds no path of its own.
    Unix(Option<PathBuf>),
}

/// The client certificate facts, present only when one was presented (mTLS). Grouped so
/// the contract's "all null unless one was presented" is a type fact.
#[derive(Debug, Clone)]
pub struct ClientCert {
    /// Serial number, hex.
    pub serial: String,
    /// Subject O; absent in some certificates.
    pub organization: Option<String>,
    /// SHA-256 over the DER form, lowercase hex.
    pub fingerprint: String,
}

/// What the TLS handshake settled, when this listener terminated TLS itself. No Default
/// on purpose: a Tls value must come from a real handshake.
#[derive(Debug, Clone)]
pub struct Tls {
    /// Protocol version as the TLS stack names it: "TLSv1.3". Must be non-empty.
    pub version: String,
    /// Negotiated cipher suite: "TLS_AES_256_GCM_SHA384". Must be non-empty.
    pub cipher: String,
    /// What ALPN settled on ("h2"), or None when the client offered no list.
    /// Maps to `Tls::$negotiatedProtocol`.
    pub alpn: Option<String>,
    /// Name the client asked for through SNI, or None if it sent none.
    /// Maps to `Tls::$requestedServerName`.
    pub server_name: Option<String>,
    pub cert: Option<ClientCert>,
}

/// A request the host rejected before dispatch; the extension answers it in its own
/// protocol. Surfaces as [`Php::exec`]'s error — downcast from `anyhow::Error`.
#[derive(Debug)]
pub struct Rejected {
    /// 400 for a malformed body, 413 past a configured limit.
    pub status: u16,
    /// For the extension's log; never sent to the client verbatim.
    pub reason: String,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status, self.reason)
    }
}

impl std::error::Error for Rejected {}

/// A request an extension runs through PHP. Pool-internal fields (`query`,
/// `content_type`, script paths) are derived by the host's backend.
///
/// Every fidelity field is populated or honestly omitted (`Option`/enum) — an extension
/// with no wire form for a fact passes None, never a fabricated default.
pub struct Request {
    pub method: String,
    pub uri: String, // path + optional ?query → REQUEST_URI
    /// The request-target byte-for-byte: the h1 request line's target, `:path` on h2/h3.
    /// None when the extension has no wire form; the host falls back to `uri`'s bytes.
    pub target: Option<Vec<u8>>,
    /// The authority the client named, byte-for-byte (`Host` on h1,
    /// `:authority` on h2/h3). None = named none; never `Some(b"")`. Rejecting
    /// a Host-less HTTP/1.1 request is the extension's job (RFC 9112 §3.2,
    /// https://www.rfc-editor.org/rfc/rfc9112#section-3.2).
    pub authority: Option<Vec<u8>>,
    pub https: bool,
    pub protocol: String, // "HTTP/1.1"
    /// The peer's end of the connection, as the socket reports it.
    pub remote: Addr,
    /// The accepting socket — which listener took the call, not configuration.
    pub server: Addr,
    /// Configured CGI facts (`SERVER_NAME`/`SERVER_PORT`, RFC 3875) and the `$uri`
    /// synthesis fallback; distinct from the socket-derived `server`.
    pub server_name: String,
    pub server_port: u16,
    /// What the handshake settled, when this listener terminated TLS itself. None on a
    /// plaintext listener — `https` may still be true behind a terminating front.
    pub tls: Option<Tls>,
    /// Unix seconds when the extension accepted the request: after the head was parsed,
    /// before the body was read. None when the extension has no ingress stamp; the host
    /// then stamps at intake.
    pub received_at: Option<f64>,
    /// One entry per field line: repeats stay separate entries in wire order,
    /// names as received (case preserved). Values are raw bytes
    /// (latin1/binary-safe). The host folds repeats only for the `$_SERVER`
    /// mapping; nothing is folded on the dispatcher path.
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
}

// No Default: it would mint status 0, which no wire can carry.
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>, // bytes: latin1/binary-safe
    pub body: Vec<u8>,
}
