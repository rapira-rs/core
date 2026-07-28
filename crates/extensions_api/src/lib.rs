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
    /// closed without a frame), or when PHP errored after it began writing its
    /// body (so the body may be incomplete).
    pub async fn exec(&self, req: Request) -> Result<Response> {
        self.backend.exec(req).await
    }
}

/// A request an extension runs through PHP. Pool-internal fields (`query`,
/// `content_type`, script paths) are derived by the host's backend.
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
    ///
    /// At most one entry per field name: combine a field's repeats before submitting —
    /// a comma list, or `"; "` for `Cookie` (RFC 9110 §5.3). A repeated name reaches
    /// `$_SERVER` as the last value alone, not as the list.
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
}

#[derive(Default)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>, // bytes: latin1/binary-safe
    pub body: Vec<u8>,
}
