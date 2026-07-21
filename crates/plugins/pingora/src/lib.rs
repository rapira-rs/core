//! Rapira HTTP front: a Pingora server that terminates HTTP and streams each request
//! through PHP via the extension `Php` bridge.
//!
//! The extension-host runtime does not enable IO, so the server runs on its own
//! IO-enabled runtime on a dedicated thread; `shutdown` flips a watch channel to drain it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use extension_api::{Extension, ListenAddr, Php, PrepareCtx, PreparedListener, Request, Result};
use pingora::http::ResponseHeader;
use pingora::proxy::{ProxyHttp, Session, http_proxy_service};
use pingora::server::configuration::ServerConf;
use pingora::server::{Fds, ListenFds};
use pingora::services::Service as _; // brings start_service() into scope
use pingora::upstreams::peer::HttpPeer;
use pingora::{Error, ErrorType, Result as PingoraResult};
use tokio::runtime::{self, Builder};
use tokio::sync::{oneshot, watch};

/// In-flight requests get this long to finish after the accept loop stops —
/// start_service only joins the accept loops, never the per-connection tasks, and
/// dropping the runtime aborts them mid-response. Kept under the host's 30s grace.
const DRAIN_GRACE: Duration = Duration::from_secs(25);

/// Where the HTTP front binds. Structurally mirrors rapira's config-side listen type,
/// but owned here so this extension crate never depends on core's config crate.
#[derive(Clone, Debug)]
pub enum Listen {
    Tcp(std::net::SocketAddr),
    Unix(std::path::PathBuf),
}

/// Configuration supplied by rapira (rapira.toml / CLI) via [`Extension::init`].
#[derive(Clone)]
pub struct Config {
    /// Address to bind: TCP socket or unix domain socket.
    pub listen: Listen,
    /// `SERVER_NAME` reported to PHP.
    pub server_name: String,
    /// `SERVER_PORT` reported to PHP.
    pub server_port: u16,
    /// Maximum request body size in bytes; larger bodies are rejected with 413.
    /// Default mirrors PHP's own `post_max_size` default (8M).
    pub max_body_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Standalone fallback only — rapira always passes a fully populated Config.
            listen: Listen::Tcp(std::net::SocketAddr::from(([127, 0, 0, 1], 8000))),
            server_name: "localhost".to_owned(),
            server_port: 8000,
            max_body_size: 8 * 1024 * 1024,
        }
    }
}

/// The HTTP front extension: holds the running server thread and its shutdown signal.
pub struct HttpServer {
    config: Config,
    /// Master-bound listener from `prepare`; rides inside `self` across the
    /// fork, consumed by `run` in the worker. None → self-bind (tests,
    /// single-process boots without a prepare phase).
    prepared: Option<PreparedListener>,
    shutdown: Option<watch::Sender<bool>>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl Extension for HttpServer {
    /// Built by rapira from its validated TOML/CLI config.
    type Config = Config;

    fn init(config: Config) -> Self {
        Self {
            config,
            prepared: None,
            shutdown: None,
            thread: None,
        }
    }

    fn name(&self) -> &str {
        "rapira-pingora"
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx) -> Result<()> {
        let prepared = match &self.config.listen {
            Listen::Tcp(addr) => ctx.bind_tcp(*addr)?,
            Listen::Unix(path) => ctx.bind_unix(path)?,
        };
        log::info!(
            "[rapira-pingora] prepared listener on {}",
            prepared.addr_string()?
        );
        self.prepared = Some(prepared);
        Ok(())
    }

    async fn run(&mut self, php: Php) -> Result<()> {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        // Wakes `run` (on the host runtime) once the server thread finishes.
        let (done_tx, done_rx) = oneshot::channel();
        let config = self.config.clone();
        let prepared = self.prepared.take();

        let thread = std::thread::Builder::new()
            .name("rapira-pingora".into())
            .spawn(move || {
                let rt: runtime::Runtime = Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(2)
                    .thread_name("rapira-pingora-io")
                    .build()
                    .expect("build http runtime");
                let result = rt.block_on(serve(php, config, prepared, shutdown_rx));
                let _ = done_tx.send(()); // ignore if `run` was already cancelled
                result
            })?;

        self.shutdown = Some(shutdown_tx);
        self.thread = Some(thread);

        // Park until the server stops on its own (bind error / clean exit). If the host
        // cancels `run`, this future is dropped and `shutdown` drains the thread instead.
        let _ = done_rx.await;

        match self.thread.take() {
            Some(thread) => join_thread(thread),
            None => Ok(()),
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(true); // stop the accept loop, drain in-flight conns
        }
        if let Some(thread) = self.thread.take() {
            // Join off the async runtime so a slow drain never blocks a worker thread.
            tokio::task::spawn_blocking(move || join_thread(thread))
                .await
                .map_err(|e| anyhow!("http join task failed: {e}"))??;
        }
        Ok(())
    }
}

fn join_thread(thread: JoinHandle<Result<()>>) -> Result<()> {
    thread.join().map_err(|payload| {
        let msg = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("unknown panic");
        anyhow!("http server thread panicked: {msg}")
    })?
}

/// Build the Pingora proxy service and drive its accept loop until `shutdown` flips true,
/// then wait (bounded) for in-flight requests before the caller drops the runtime.
async fn serve(
    php: Php,
    config: Config,
    prepared: Option<PreparedListener>,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let conf = Arc::new(ServerConf::default());
    let inflight = Arc::new(AtomicUsize::new(0));
    let listen = config.listen.clone();
    let mut service = http_proxy_service(
        &conf,
        PhpProxy {
            php,
            config,
            inflight: inflight.clone(),
        },
    );

    // ONE string feeds BOTH the endpoint and the Fds key: pingora adopts only on
    // an exact match (a mismatch silently rebinds — for unix sockets it would
    // unlink and steal the master's socket). The listener carries the resolved
    // address, so port 0 configs adopt correctly too.
    let (addr, fds): (ListenAddr, Option<ListenFds>) = match prepared {
        Some(p) => {
            use std::os::fd::IntoRawFd;
            let addr = p.addr().clone();
            let key = p.addr_string()?;
            let mut table = Fds::new();
            table.add(key, p.into_raw_fd()); // ownership → pingora; closed at teardown
            (addr, Some(Arc::new(tokio::sync::Mutex::new(table))))
        }
        None => (
            match &listen {
                Listen::Tcp(a) => ListenAddr::Tcp(*a),
                Listen::Unix(p) => ListenAddr::Unix(p.clone()),
            },
            None,
        ),
    };
    match &addr {
        ListenAddr::Tcp(a) => {
            let s = a.to_string();
            service.add_tcp(&s);
            log::info!("[rapira-pingora] listening on http://{s}");
        }
        ListenAddr::Unix(path) => {
            // Reject rather than bind a lossy-converted (corrupted) path.
            let s = path
                .to_str()
                .ok_or_else(|| anyhow!("unix socket path must be valid UTF-8"))?;
            // None → pingora default perms (0o666): the same local accessibility as a
            // loopback TCP bind, and a proxy running as another user can connect.
            service.add_uds(s, None);
            log::info!("[rapira-pingora] listening on unix:{s}");
        }
    }
    // (fds, shutdown, listeners_per_fd). Runs on this runtime via Handle::current().
    service.start_service(fds, shutdown, 1).await;
    // start_service only joined the accept loops; connection tasks are detached and die
    // with the runtime. Wait for the requests still in flight so their responses go out.
    let deadline = tokio::time::Instant::now() + DRAIN_GRACE;
    while inflight.load(Ordering::Acquire) > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    log::info!("[rapira-pingora] accept loop stopped");
    Ok(())
}

/// Terminates HTTP and answers every request from PHP; never proxies upstream.
struct PhpProxy {
    php: Php,
    config: Config,
    /// Requests between `new_ctx` and `logging` — `serve` drains this on shutdown.
    inflight: Arc<AtomicUsize>,
}

#[async_trait]
impl ProxyHttp for PhpProxy {
    type CTX = ();

    // Every request runs new_ctx → phases → logging, so the pair below cannot underflow.
    fn new_ctx(&self) -> Self::CTX {
        self.inflight.fetch_add(1, Ordering::AcqRel);
    }

    async fn logging(&self, _session: &mut Session, _e: Option<&Error>, _ctx: &mut Self::CTX) {
        self.inflight.fetch_sub(1, Ordering::AcqRel);
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> PingoraResult<bool> {
        let Some(request) = build_request(session, &self.config).await? else {
            return Ok(true); // rejected here; 413 already written
        };

        // Buffer the whole PHP response so we can send a real Content-Length. Without a
        // framed body (Content-Length or chunked) HTTP/1.1 falls back to close-delimiting,
        // which forces a connection close per request — no keepalive, a fresh 64 KiB
        // accept buffer every time. A Content-Length keeps connections alive.
        let response = self.php.exec(request).await.map_err(|e| {
            Error::explain(
                ErrorType::HTTPStatus(502),
                format!("php exec failed: {e:#}"),
            )
        })?;

        // A missing or informational (1xx) head can't be forwarded as a final response.
        let status = if response.status < 200 {
            502
        } else {
            response.status
        };
        // 204/304 have no message body: never add a server-framed Content-Length (a
        // forced 0 would misframe a 304). PHP's own Content-Length is dropped by
        // skip_response_header like on any response.
        let no_body = matches!(status, 204 | 304);

        let mut header = ResponseHeader::build(status, Some(response.headers.len() + 1))?;
        // Extra hop-by-hop fields named by a Connection value (RFC 9110 §7.6.1,
        // https://www.rfc-editor.org/rfc/rfc9110#section-7.6.1). PHP almost never
        // sends Connection, so this stays empty and allocates nothing.
        let mut conn_named: Vec<String> = Vec::new();
        for (name, value) in response.headers {
            if name.eq_ignore_ascii_case("connection") {
                connection_named_headers(&value, &mut conn_named);
                continue; // Connection is itself hop-by-hop
            }
            // Framing is derived from the buffered body and connection management is
            // ours, never PHP's (hop-by-hop, RFC 9110 §7.6.1).
            if skip_response_header(&name) {
                continue;
            }
            // append: PHP may legally repeat headers (Set-Cookie, Vary, Link).
            header.append_header(name, value)?; // value: Vec<u8>, binary-safe
        }
        // Rare path only: drop the fields a Connection value named, before our own
        // Content-Length goes in below so a `Connection: content-length` can't strip it.
        for tok in &conn_named {
            header.remove_header(tok.as_str());
        }
        if !no_body {
            header.insert_header("Content-Length", response.body.len().to_string())?;
        }
        session
            .write_response_header(Box::new(header), no_body)
            .await?;
        if !no_body {
            session
                .write_response_body(Some(response.body.into()), true)
                .await?;
        }
        Ok(true) // response already sent; proxy runs logging + finish, never reaches upstream
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> PingoraResult<Box<HttpPeer>> {
        // Never reached: request_filter always answers and returns Ok(true).
        Err(Error::explain(
            ErrorType::InternalError,
            "rapira-pingora serves all requests locally; no upstream",
        ))
    }
}

/// Parse a `Connection` header value into the lower-cased field names it lists,
/// appending them to `out` (RFC 9110 §7.6.1). Values are binary-safe, so a lossy
/// UTF-8 decode is used only to tokenize; empty tokens are skipped.
pub fn connection_named_headers(value: &[u8], out: &mut Vec<String>) {
    for tok in value.split(|&b| b == b',') {
        let tok = String::from_utf8_lossy(tok).trim().to_ascii_lowercase();
        if !tok.is_empty() {
            out.push(tok);
        }
    }
}

/// Headers this front owns instead of PHP: framing comes from the buffered body and
/// connection management belongs to the server (hop-by-hop, RFC 9110 §7.6.1).
pub fn skip_response_header(name: &str) -> bool {
    [
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "upgrade",
        "trailer",
        "te",
        "proxy-connection",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

/// Map a Pingora downstream request into a rapira `Request`. `None` means the request
/// was rejected here (413 already written).
async fn build_request(session: &mut Session, config: &Config) -> PingoraResult<Option<Request>> {
    let header: &pingora::prelude::RequestHeader = session.req_header();
    let method: String = header.method.as_str().to_owned();
    let uri: String = header.uri.to_string(); // path + ?query → REQUEST_URI
    // → SERVER_PROTOCOL, e.g. "HTTP/1.1". The framework-type → CGI-string mapping
    // lives here, not in core; static strings for the common versions (the Debug
    // formatter shows up in per-request profiles).
    let v = header.version;
    let protocol: String = match v {
        pingora::http::Version::HTTP_11 => "HTTP/1.1".to_owned(),
        pingora::http::Version::HTTP_10 => "HTTP/1.0".to_owned(),
        pingora::http::Version::HTTP_2 => "HTTP/2.0".to_owned(),
        pingora::http::Version::HTTP_3 => "HTTP/3.0".to_owned(),
        _ => format!("{v:?}"),
    };
    let headers: Vec<(String, Vec<u8>)> = header
        .headers
        .iter()
        // Header values pass through as raw bytes — a lossy UTF-8 decode here would
        // corrupt latin1/binary values (e.g. a signed header, a latin1 cookie) that
        // PHP must see verbatim, exactly as the response path already preserves them.
        .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
        .collect();

    let declared_len = header
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    // Send the interim 100 only for HTTP/1.1. Per RFC 9110 §10.1.1
    // (https://www.rfc-editor.org/rfc/rfc9110#section-10.1.1) a server MUST ignore an
    // HTTP/1.0 request's 100-continue expectation; h2/h3 handle interim responses over
    // their own framing and aren't driven from this path.
    let expects_continue = header.version == pingora::http::Version::HTTP_11
        && header
            .headers
            .get("expect")
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"100-continue"));

    if declared_len.is_some_and(|len| len > config.max_body_size) {
        reject_payload_too_large(session).await?;
        return Ok(None);
    }
    // The client holds the body back until the interim response acknowledges Expect.
    if expects_continue {
        session.write_continue_response().await?;
    }

    let (remote_addr, remote_port) = session
        .client_addr()
        .and_then(|addr| addr.as_inet())
        .map(|inet| (inet.ip().to_string(), inet.port()))
        .unwrap_or_else(|| ("127.0.0.1".to_owned(), 0));

    // Pre-size to the validated Content-Length (already ≤ max_body_size); chunked
    // bodies with no declared length keep growth-by-doubling.
    let mut body: Vec<u8> = Vec::with_capacity(declared_len.unwrap_or(0));
    while let Some(chunk) = session.read_request_body().await? {
        // Counting covers chunked bodies that declared no length up front.
        if body.len() + chunk.len() > config.max_body_size {
            reject_payload_too_large(session).await?;
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }

    Ok(Some(Request {
        method,
        uri,
        https: false, // plaintext for now; set from TLS once terminated here
        protocol,
        remote_addr,
        remote_port,
        server_name: config.server_name.clone(),
        server_port: config.server_port,
        headers,
        body,
    }))
}

/// 413 + close: the unread body is still on the wire, so the connection can't be reused.
async fn reject_payload_too_large(session: &mut Session) -> PingoraResult<()> {
    let mut header = ResponseHeader::build(413, Some(1))?;
    header.insert_header("Content-Length", "0")?;
    session.set_keepalive(None);
    session
        .write_response_header(Box::new(header), true)
        .await?;
    Ok(())
}
