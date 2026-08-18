use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

mod prepare;
pub use prepare::{LISTEN_BACKLOG, ListenAddr, PrepareCtx, PreparedListener};

pub type Result<T = (), E = anyhow::Error> = std::result::Result<T, E>;

/// One entry per field line: repeats stay separate, wire order, names as received, values raw bytes (latin1/binary-safe).
pub type FieldLines = Vec<(String, Vec<u8>)>;

/// Lifecycle: `init` → `prepare` (master-side, pre-fork) → `run` → `shutdown`; the host drops the in-flight `run` future before it calls `shutdown`.
pub trait Extension: Send + 'static {
    type Config;

    fn init(config: Self::Config) -> Self
    where
        Self: Sized;

    fn name(&self) -> &str;

    /// Master-side pre-fork hook: synchronous, no runtime exists; resources stored in `self` cross the fork and `run` consumes them in the worker.
    fn prepare(&mut self, _ctx: &mut PrepareCtx) -> Result<()> {
        Ok(())
    }

    /// Must reach `.await` points regularly: the host stops `run` only by cancelling it.
    fn run(&mut self, php: Php) -> impl Future<Output = Result<()>> + Send;

    fn shutdown(&mut self) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
}

#[doc(hidden)]
pub trait Backend: Send + Sync + 'static {
    fn exec(&self, req: Request) -> Pin<Box<dyn Future<Output = Result<Reply>> + Send + '_>>;
}

/// Wire order `Interim* Head? (Chunk|File)* End?`: a stream ending without `End` means the worker died, without `Head` that it produced no response at all.
pub enum ReplyEvent {
    Interim {
        status: u16,
        headers: FieldLines,
    },
    Head {
        status: u16,
        headers: FieldLines,
        /// None means the extension chooses the framing (chunked on HTTP/1.1).
        content_length: Option<u64>,
        /// Send no body bytes and no framing fields, whatever else the stream carries.
        bodiless: bool,
        /// The body is already content-coded; compression must leave it alone.
        body_coded: bool,
    },
    Chunk(bytes::Bytes),
    File {
        file: std::fs::File,
        offset: u64,
        len: u64,
    },
    End {
        trailers: FieldLines,
        truncated: bool,
    },
}

#[doc(hidden)]
pub trait ReplySource: Send + 'static {
    fn next(&mut self) -> Pin<Box<dyn Future<Output = Option<ReplyEvent>> + Send + '_>>;
}

/// Dropping it tells the host the client is gone: the worker's next write raises `WorkDiscardedException`.
pub struct Reply(Box<dyn ReplySource>);

impl Reply {
    #[doc(hidden)]
    pub fn new(source: Box<dyn ReplySource>) -> Self {
        Self(source)
    }

    pub async fn next(&mut self) -> Option<ReplyEvent> {
        self.0.next().await
    }

    pub async fn collect(mut self) -> Result<Response> {
        let mut response: Option<Response> = None;
        let mut end: Option<bool> = None;
        while let Some(ev) = self.0.next().await {
            match ev {
                ReplyEvent::Interim { .. } => {}
                ReplyEvent::Head {
                    status, headers, ..
                } => {
                    response = Some(Response {
                        status,
                        headers,
                        body: Vec::new(),
                    });
                }
                ReplyEvent::Chunk(b) => {
                    if let Some(r) = response.as_mut() {
                        r.body.extend_from_slice(&b);
                    }
                }
                ReplyEvent::File { file, offset, len } => {
                    if let Some(r) = response.as_mut() {
                        r.body.extend_from_slice(&read_slice(&file, offset, len)?);
                    }
                }
                ReplyEvent::End { truncated, .. } => {
                    end = Some(truncated);
                    break;
                }
            }
        }
        match (response, end) {
            (None, None) => Err(anyhow::anyhow!(
                "php worker died mid-response (channel closed without a response)"
            )),
            (Some(_), None) | (_, Some(true)) => {
                Err(anyhow::anyhow!("php crashed mid-response; body truncated"))
            }
            (None, Some(false)) => Err(anyhow::anyhow!("php produced no response head")),
            (Some(r), Some(false)) => Ok(r),
        }
    }
}

fn read_slice(file: &std::fs::File, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut out = vec![0u8; usize::try_from(len).unwrap_or(usize::MAX)];
    let mut done = 0usize;
    while done < out.len() {
        let n = file.read_at(&mut out[done..], offset + done as u64)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    out.truncate(done);
    Ok(out)
}

/// Every clone shares the host's backend handle: never keep a spare past `run`/`shutdown`, the host's shutdown contract needs them all dropped.
#[derive(Clone)]
pub struct Php {
    backend: Arc<dyn Backend>,
}

impl Php {
    #[doc(hidden)]
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    /// A pre-dispatch refusal errors with a downcastable [`Rejected`]; response-shape failures surface from [`Reply::next`]/[`Reply::collect`].
    pub async fn exec(&self, req: Request) -> Result<Reply> {
        self.backend.exec(req).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addr {
    Inet(std::net::SocketAddr),
    /// None is an unnamed endpoint: the usual case for a peer on a unix listener, which binds no path of its own.
    Unix(Option<PathBuf>),
}

#[derive(Debug, Clone)]
pub struct ClientCert {
    pub serial: String,
    pub organization: Option<String>,
    /// SHA-256 over the DER form, lowercase hex.
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct Tls {
    pub version: String,
    pub cipher: String,
    /// Maps to `Tls::$negotiatedProtocol`; None when the client offered no ALPN list.
    pub alpn: Option<String>,
    /// Maps to `Tls::$requestedServerName`; None when the client sent no SNI.
    pub server_name: Option<String>,
    pub cert: Option<ClientCert>,
}

#[derive(Debug)]
pub struct Rejected {
    pub status: u16,
    pub reason: String,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status, self.reason)
    }
}

impl std::error::Error for Rejected {}

/// An extension with no wire form for a fidelity fact passes None, never a fabricated default.
pub struct Request {
    pub method: String,
    pub uri: String,
    /// The request-target byte-for-byte (h1 request line, `:path` on h2/h3); None makes the host fall back to `uri`'s bytes.
    pub target: Option<Vec<u8>>,
    /// None = the client named no authority, never `Some(b"")`; rejecting a Host-less HTTP/1.1 request is the extension's job (RFC 9112 §3.2).
    /// https://www.rfc-editor.org/rfc/rfc9112#section-3.2
    pub authority: Option<Vec<u8>>,
    pub https: bool,
    pub protocol: String,
    pub remote: Addr,
    /// The accepting socket: which listener took the call, not configuration.
    pub server: Addr,
    /// Configured CGI facts (`SERVER_NAME`/`SERVER_PORT`, RFC 3875), distinct from the socket-derived `server`.
    pub server_name: String,
    pub server_port: u16,
    /// None on a plaintext listener: `https` may still be true behind a terminating front.
    pub tls: Option<Tls>,
    /// Unix seconds stamped at accept, head parsed and body not yet read; None makes the host stamp at intake.
    pub received_at: Option<f64>,
    pub headers: FieldLines,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: FieldLines,
    pub body: Vec<u8>,
}
