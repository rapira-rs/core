//! Rapira HTTP front: a Pingora server that terminates HTTP and streams each request
//! through PHP via the extension `Php` bridge.
//!
//! The extension runtime does not enable IO, so the server runs on its own
//! IO-enabled runtime on a dedicated thread; `shutdown` flips a watch channel to drain it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use extension_api::{Extension, ListenAddr, Php, PrepareCtx, PreparedListener, Request, Result};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::modules::http::compression::ResponseCompressionBuilder;
use pingora::modules::http::{HttpModule, HttpModuleBuilder, HttpModules, Module};
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
    /// What to do with a request field whose name is not [`is_safe_field_name`].
    pub unsafe_field_names: UnsafeFieldNames,
    /// The pool serves superglobals ($_SERVER) — classic/worker mode. The Drop
    /// policy protects that mapping only, so it is inert without it; Reject is
    /// an explicit opt-in and applies regardless.
    pub superglobals: bool,
}

/// What to do with a request field name that maps onto a CGI variable another name
/// already owns. Structurally mirrors rapira's config-side type, but owned here so this
/// extension crate never depends on core's config crate.
///
/// There is deliberately no "allow" arm: a boolean off-switch re-opens the
/// aliasing collision (CVE-2026-52845 is that bug shipped). An allowlist of
/// specific expected names could be safe; a boolean cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsafeFieldNames {
    /// Remove the field before anything downstream sees it.
    Drop,
    /// Answer 400 and serve nothing.
    Reject,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Standalone fallback only — rapira always passes a fully populated Config.
            listen: Listen::Tcp(std::net::SocketAddr::from(([127, 0, 0, 1], 8000))),
            server_name: "localhost".to_owned(),
            server_port: 8000,
            max_body_size: 8 * 1024 * 1024,
            unsafe_field_names: UnsafeFieldNames::Drop,
            superglobals: true,
        }
    }
}

/// Whether a request field name reaches a CGI variable no other name can.
///
/// The CGI name is the field name uppercased with `-` rewritten to `_`, and PHP rewrites
/// `.` and space to `_` again when it registers the variable. A name carrying `_` or `.`
/// therefore lands on the variable a `-` name owns, letting a client fold a value into a
/// field the front set. (Space cannot reach here — it is not a tchar, so the parser rejects
/// it first.) This is an allowlist rather than a denylist of those two bytes, so it stays
/// correct if either mapper widens.
fn is_safe_field_name(name: &str) -> bool {
    name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Applies [`UnsafeFieldNames`] to the downstream request. Serves as both the
/// builder and the per-request module: `init` copies two plain fields.
struct FieldNameFilter {
    policy: UnsafeFieldNames,
    /// Drop protects the $_SERVER mapping only; without one, deleting
    /// tchar-valid names would contradict the contract's "names as received".
    superglobals: bool,
}

#[async_trait]
impl HttpModule for FieldNameFilter {
    async fn request_header_filter(&mut self, req: &mut RequestHeader) -> PingoraResult<()> {
        if self.policy == UnsafeFieldNames::Drop && !self.superglobals {
            return Ok(());
        }
        // Collected first: `remove_header` needs the map mutably, and it drops every value
        // under the name along with its case-preserving entry.
        let unsafe_names: Vec<String> = req
            .headers
            .keys()
            .filter(|name| !is_safe_field_name(name.as_str()))
            .map(|name| name.as_str().to_owned())
            .collect();
        if unsafe_names.is_empty() {
            return Ok(());
        }
        if self.policy == UnsafeFieldNames::Reject {
            // Only the count and one example: pingora logs this at error level (the default
            // filter's only visible level), so joining every name lets one request with a
            // few hundred junk fields write a few hundred KB of log.
            return Err(Error::explain(
                ErrorType::HTTPStatus(400),
                format!(
                    "{} field name(s) alias a CGI variable, e.g. {}",
                    unsafe_names.len(),
                    unsafe_names[0]
                ),
            ));
        }
        for name in &unsafe_names {
            req.remove_header(name.as_str());
            // warn, not debug: the default filter prints errors only, so a debug line here
            // makes a dropped client field indistinguishable from one never sent.
            tracing::warn!(
                target: "http",
                "dropped request header {name}: name aliases a CGI variable \
                 (unsafe_field_names = \"drop\"; use \"reject\" to answer 400 instead)"
            );
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl HttpModuleBuilder for FieldNameFilter {
    fn init(&self) -> Module {
        Box::new(Self {
            policy: self.policy,
            superglobals: self.superglobals,
        })
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
        tracing::info!(target: "http", "prepared listener on {}", prepared.addr_string()?);
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
    let conf: Arc<ServerConf> = Arc::new(ServerConf::default());
    let inflight: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let listen = config.listen.clone();

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
    let mut service = http_proxy_service(
        &conf,
        PhpProxy {
            php,
            config,
            // the resolved bind: the $server fallback and the peer-arm kind
            listen: addr.clone(),
            inflight: inflight.clone(),
        },
    );
    match &addr {
        ListenAddr::Tcp(a) => {
            let s = a.to_string();
            service.add_tcp(&s);
            tracing::info!(target: "http", "listening on http://{s}");
        }
        ListenAddr::Unix(path) => {
            // Reject rather than bind a lossy-converted (corrupted) path.
            let s = path
                .to_str()
                .ok_or_else(|| anyhow!("unix socket path must be valid UTF-8"))?;
            // None → pingora default perms (0o666): the same local accessibility as a
            // loopback TCP bind, and a proxy running as another user can connect.
            service.add_uds(s, None);
            tracing::info!(target: "http", "listening on unix:{s}");
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
    // Falling out of the loop on the deadline means the caller is about to drop the runtime
    // out from under requests that are still running, cutting their responses mid-flight.
    // Reporting Ok here would surface that as a clean stop with exit code 0.
    let stranded = inflight.load(Ordering::Acquire);
    if stranded > 0 {
        return Err(anyhow!(
            "http drain timed out after {DRAIN_GRACE:?} with {stranded} request(s) in flight; \
             their responses were cut short"
        ));
    }
    tracing::info!(target: "http", "drained cleanly; accept loop stopped");
    Ok(())
}

/// Terminates HTTP and answers every request from PHP; never proxies upstream.
struct PhpProxy {
    php: Php,
    config: Config,
    /// The resolved bind address (port-0 resolved): the `$server` fallback and
    /// the kind key for a peerless connection.
    listen: ListenAddr,
    /// Requests between `new_ctx` and `logging` — `serve` drains this on shutdown.
    inflight: Arc<AtomicUsize>,
}

/// Per-request state; created after the head is parsed, before any body byte.
pub struct ReqCtx {
    /// Unix seconds when this host accepted the request.
    received_at: f64,
}

#[async_trait]
impl ProxyHttp for PhpProxy {
    type CTX = ReqCtx;

    // Every request runs new_ctx → phases → logging, so the inflight pair cannot underflow.
    fn new_ctx(&self) -> Self::CTX {
        self.inflight.fetch_add(1, Ordering::AcqRel);
        ReqCtx {
            received_at: std::time::UNIX_EPOCH
                .elapsed()
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
        }
    }

    /// Registered once when the service is built. The module's `request_header_filter` then
    /// runs per request ahead of `request_filter`, so the field-name policy is applied while
    /// the request is still a header map and every later phase sees a screened one.
    fn init_downstream_modules(&self, modules: &mut HttpModules) {
        // The trait default supplies this and an override replaces the whole body, so it
        // has to be re-added or downstream compression loses its (disabled) module.
        modules.add_module(ResponseCompressionBuilder::enable(0));
        modules.add_module(Box::new(FieldNameFilter {
            policy: self.config.unsafe_field_names,
            superglobals: self.config.superglobals,
        }));
    }

    async fn logging(&self, _session: &mut Session, _e: Option<&Error>, _ctx: &mut Self::CTX) {
        self.inflight.fetch_sub(1, Ordering::AcqRel);
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<bool> {
        let Some(request) = build_request(session, &self.config, &self.listen, ctx).await? else {
            return Ok(true); // rejected here; the response is already written
        };

        // Buffer the whole PHP response so we can send a real Content-Length. Without a
        // framed body (Content-Length or chunked) HTTP/1.1 falls back to close-delimiting,
        // which forces a connection close per request — no keepalive, a fresh 64 KiB
        // accept buffer every time. A Content-Length keeps connections alive.
        let response: extension_api::Response = match self.php.exec(request).await {
            Ok(r) => r,
            // host-rejected before dispatch: answer in this protocol
            Err(e) if e.downcast_ref::<extension_api::Rejected>().is_some() => {
                let r = e.downcast_ref::<extension_api::Rejected>().unwrap();
                tracing::warn!(target: "http", "rejected before dispatch: {r}");
                let mut header = ResponseHeader::build(r.status, Some(1))?;
                header.insert_header("Content-Length", "0")?;
                session.set_keepalive(None);
                session
                    .write_response_header(Box::new(header), true)
                    .await?;
                return Ok(true);
            }
            Err(e) => {
                // a spool io failure is a host fault, not a bad gateway
                let status = if e.chain().any(|c| c.is::<std::io::Error>()) {
                    500
                } else {
                    502
                };
                return Err(Error::explain(
                    ErrorType::HTTPStatus(status),
                    format!("php exec failed: {e:#}"),
                ));
            }
        };

        // An informational head can't be forwarded as a final response over h1
        // (the contract's 101 carve-out included: this front has no upgrade
        // machinery). Loud, or the rewrite is invisible outside the wire.
        let status = if response.status < 200 {
            tracing::error!(
                target: "http",
                "php committed status {} as final; this front cannot forward it — serving 502",
                response.status
            );
            502
        } else {
            response.status
        };
        // 204/304 have no message body: never add a server-framed Content-Length (a
        // forced 0 would misframe a 304). A HEAD response body never goes on the
        // wire either, and the buffered length is not what a GET would have sent
        // (RFC 9110 §8.6), so send neither body nor Content-Length there.
        // https://www.rfc-editor.org/rfc/rfc9110#section-8.6
        let no_body = matches!(status, 204 | 304)
            || session.req_header().method == pingora::http::Method::HEAD;

        let header = build_response_header(status, response.headers, response.body.len(), no_body)?;
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

/// Assemble the response head: PHP's fields minus the ones this front owns, then our
/// own framing. A field no front can represent is dropped with a log rather than
/// failing the call — the head carries the whole response, so one bad field must not
/// cost the body.
fn build_response_header(
    status: u16,
    headers: Vec<(String, Vec<u8>)>,
    body_len: usize,
    no_body: bool,
) -> PingoraResult<ResponseHeader> {
    let mut header = ResponseHeader::build(status, Some(headers.len() + 1))?;
    // Extra hop-by-hop fields named by a Connection value (RFC 9110 §7.6.1,
    // https://www.rfc-editor.org/rfc/rfc9110#section-7.6.1). PHP almost never
    // sends Connection, so this stays empty and allocates nothing.
    let mut conn_named: Vec<String> = Vec::new();
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("connection") {
            connection_named_headers(&value, &mut conn_named);
            continue; // Connection is itself hop-by-hop
        }
        // Framing is derived from the buffered body and connection management is
        // ours, never PHP's (hop-by-hop, RFC 9110 §7.6.1).
        if skip_response_header(&name) {
            continue;
        }
        // append: PHP may legally repeat headers (Set-Cookie, Vary, Link). value is
        // Vec<u8>, so it stays binary-safe.
        // Cloned for the log because append_header takes the name by value and pingora has
        // no IntoCaseHeaderName impl for a non-'static &str — it cannot be borrowed.
        let logged = name.clone();
        if let Err(e) = header.append_header(name, value) {
            tracing::debug!(target: "http", "dropped response header {logged}: {e}");
        }
    }
    // Rare path only: drop the fields a Connection value named, before our own
    // Content-Length goes in below so a `Connection: content-length` can't strip it.
    for tok in &conn_named {
        header.remove_header(tok.as_str());
    }
    if !no_body {
        header.insert_header("Content-Length", body_len.to_string())?;
    }
    Ok(header)
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

/// One entry per field line, wire order per name. `case_header_iter` yields
/// nothing without a case map (h2's `From<ReqParts>` sets none), so fall back
/// to the lowercase map — which is what h2/h3 put on the wire anyway.
fn collect_headers(header: &RequestHeader) -> Vec<(String, Vec<u8>)> {
    if header.has_case() {
        header
            .case_header_iter()
            .map(|(n, v)| {
                (
                    String::from_utf8_lossy(n.as_slice()).into_owned(),
                    v.as_bytes().to_vec(),
                )
            })
            .collect()
    } else {
        header
            .headers
            .iter()
            .map(|(n, v)| (n.as_str().to_owned(), v.as_bytes().to_vec()))
            .collect()
    }
}

/// `$authority` from the Host field lines. RFC 9112 §3.2 makes a repeated,
/// missing or empty Host on HTTP/1.1 a 400; without a usable Host the target
/// URI reconstructs with an empty authority (RFC 9112 §3.3), named None here.
/// h2/h3 carry the authority as `:authority` in the URI, not a Host line —
/// unreachable on today's plaintext-h1 listeners, to be read from
/// `header.uri.authority()` when a TLS/h2 front lands.
/// https://www.rfc-editor.org/rfc/rfc9112#section-3.2
/// https://www.rfc-editor.org/rfc/rfc9112#section-3.3
fn authority(headers: &[(String, Vec<u8>)], http11: bool) -> Result<Option<Vec<u8>>, &'static str> {
    let mut lines = headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v);
    let first = lines.next();
    if lines.next().is_some() {
        return Err("request carries more than one Host field line");
    }
    match first {
        Some(v) if !v.is_empty() => Ok(Some(v.clone())),
        Some(_) if http11 => Err("HTTP/1.1 request with an empty Host field value"),
        None if http11 => Err("HTTP/1.1 request without a Host field"),
        _ => Ok(None),
    }
}

fn peer_addr(a: &pingora::protocols::l4::socket::SocketAddr) -> extension_api::Addr {
    match a {
        pingora::protocols::l4::socket::SocketAddr::Inet(sa) => extension_api::Addr::Inet(*sa),
        pingora::protocols::l4::socket::SocketAddr::Unix(u) => {
            extension_api::Addr::Unix(u.as_pathname().map(Into::into))
        }
    }
}

fn listen_addr(listen: &ListenAddr) -> extension_api::Addr {
    match listen {
        ListenAddr::Tcp(a) => extension_api::Addr::Inet(*a),
        ListenAddr::Unix(p) => extension_api::Addr::Unix(Some(p.clone())),
    }
}

/// Map a Pingora downstream request into a rapira `Request`. `None` means the request
/// was rejected here (the response is already written).
async fn build_request(
    session: &mut Session,
    config: &Config,
    listen: &ListenAddr,
    ctx: &ReqCtx,
) -> PingoraResult<Option<Request>> {
    let header: &RequestHeader = session.req_header();
    let method: String = header.method.as_str().to_owned();
    // byte-exact request-target (raw_path keeps non-UTF-8 bytes); uri stays the
    // lossy string for REQUEST_URI
    let target: Vec<u8> = header.raw_path().to_vec();
    let uri: String = header.uri.to_string();
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
    let headers: Vec<(String, Vec<u8>)> = collect_headers(header);
    let authority = authority(&headers, v == pingora::http::Version::HTTP_11)
        .map_err(|reason| Error::explain(ErrorType::HTTPStatus(400), reason))?;

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

    let remote = match session.client_addr() {
        Some(a) => peer_addr(a),
        // None: an unnamed UDS peer (the normal case there), or a TCP session
        // whose digest is gone — the contract union has no unknown arm, so the
        // latter is answered, not fabricated
        None => match listen {
            ListenAddr::Unix(_) => extension_api::Addr::Unix(None),
            ListenAddr::Tcp(_) => {
                return Err(Error::explain(
                    ErrorType::HTTPStatus(500),
                    "connection carries no peer address",
                ));
            }
        },
    };
    // real getsockname: the concrete local address on a wildcard bind
    let server = match session.server_addr() {
        Some(a) => peer_addr(a),
        None => listen_addr(listen),
    };

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
        target: Some(target),
        authority,
        https: false, // plaintext for now; set from TLS once terminated here
        protocol,
        remote,
        server,
        server_name: config.server_name.clone(),
        server_port: config.server_port,
        tls: None, // plumb-only until a TLS backend lands
        received_at: Some(ctx.received_at),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.as_bytes().to_vec()))
            .collect()
    }

    /// The Connection-named removals must run before our own Content-Length goes in,
    /// or PHP could strip the framing out from under the body.
    #[test]
    fn connection_value_cannot_strip_framing() {
        let head = build_response_header(
            200,
            hdrs(&[
                ("Connection", "content-length, x-drop"),
                ("X-Drop", "1"),
                ("X-Keep", "2"),
            ]),
            7,
            false,
        )
        .unwrap();
        assert_eq!(head.headers.get("content-length").unwrap().as_bytes(), b"7");
        assert!(head.headers.get("x-drop").is_none());
        assert_eq!(head.headers.get("x-keep").unwrap().as_bytes(), b"2");
        assert!(head.headers.get("connection").is_none());
    }

    #[test]
    fn bodyless_statuses_get_no_content_length() {
        for status in [204u16, 304] {
            let head = build_response_header(status, hdrs(&[("X-A", "1")]), 0, true).unwrap();
            assert!(head.headers.get("content-length").is_none(), "{status}");
        }
    }

    /// A space is not a tchar, so no front can put this name on the wire. Dropping the
    /// field must not cost the rest of the response.
    #[test]
    fn unrepresentable_header_is_dropped_not_fatal() {
        let head = build_response_header(
            200,
            hdrs(&[("Content Type", "text/html"), ("X-Keep", "2")]),
            3,
            false,
        )
        .unwrap();
        assert_eq!(head.headers.get("x-keep").unwrap().as_bytes(), b"2");
        assert_eq!(head.headers.get("content-length").unwrap().as_bytes(), b"3");
    }

    #[test]
    fn php_framing_headers_never_reach_the_wire() {
        let head = build_response_header(
            200,
            hdrs(&[("Content-Length", "999"), ("Transfer-Encoding", "chunked")]),
            4,
            false,
        )
        .unwrap();
        assert_eq!(head.headers.get("content-length").unwrap().as_bytes(), b"4");
        assert!(head.headers.get("transfer-encoding").is_none());
    }

    #[test]
    fn connection_tokens_are_split_trimmed_and_lowercased() {
        let mut out = Vec::new();
        connection_named_headers(b"  Keep-Alive , ,X-Foo\t", &mut out);
        assert_eq!(out, vec!["keep-alive".to_owned(), "x-foo".to_owned()]);
    }

    fn request_with(fields: &[(&str, &str)]) -> RequestHeader {
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        for (name, value) in fields {
            header.append_header(name.to_string(), *value).unwrap();
        }
        header
    }

    fn value_of<'a>(headers: &'a [(String, Vec<u8>)], name: &str) -> Option<&'a [u8]> {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_slice())
    }

    /// One entry per field line, wire order per name, case as sent.
    #[test]
    fn headers_arrive_per_line_with_case() {
        let headers = collect_headers(&request_with(&[
            ("X-Probe", "one"),
            ("Accept", "text/*"),
            ("x-probe", "two"),
        ]));
        let probes: Vec<_> = headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("x-probe"))
            .collect();
        assert_eq!(probes.len(), 2);
        assert_eq!(probes[0].0, "X-Probe");
        assert_eq!(probes[0].1, b"one");
        assert_eq!(probes[1].0, "x-probe");
        assert_eq!(probes[1].1, b"two");
        assert_eq!(value_of(&headers, "accept"), Some(&b"text/*"[..]));
    }

    /// Without a case map (h2's From<ReqParts> path) the fallback still yields
    /// every line, lowercase.
    #[test]
    fn no_case_requests_still_yield_headers() {
        let mut header = RequestHeader::build_no_case("GET", b"/", None).unwrap();
        header.append_header("X-Foo".to_string(), "1").unwrap();
        header.append_header("X-Foo".to_string(), "2").unwrap();
        let headers = collect_headers(&header);
        let foos: Vec<_> = headers.iter().filter(|(n, _)| n == "x-foo").collect();
        assert_eq!(foos.len(), 2);
    }

    /// RFC 9112 §3.2: repeated/missing/empty Host on 1.1 → 400; a bare 1.0
    /// request named no authority.
    #[test]
    fn authority_follows_the_host_rules() {
        let one = |v: &str| vec![("Host".to_owned(), v.as_bytes().to_vec())];
        assert_eq!(
            authority(&one("a.example"), true).unwrap().as_deref(),
            Some(&b"a.example"[..])
        );
        assert!(authority(&[], true).is_err());
        assert!(authority(&one(""), true).is_err());
        let two = vec![
            ("Host".to_owned(), b"a".to_vec()),
            ("host".to_owned(), b"b".to_vec()),
        ];
        assert!(authority(&two, false).is_err());
        assert_eq!(authority(&[], false).unwrap(), None);
        assert_eq!(authority(&one(""), false).unwrap(), None);
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(f)
    }

    /// Field names surviving `policy`, in map order.
    fn surviving(policy: UnsafeFieldNames, fields: &[(&str, &str)]) -> PingoraResult<Vec<String>> {
        let mut header = request_with(fields);
        run(FieldNameFilter {
            policy,
            superglobals: true,
        }
        .request_header_filter(&mut header))?;
        Ok(header
            .headers
            .keys()
            .map(|name| name.as_str().to_owned())
            .collect())
    }

    /// Drop protects the $_SERVER mapping; a dispatcher pool has none, so the
    /// contract's "names as received" wins there.
    #[test]
    fn drop_is_inert_without_superglobals() {
        let mut header = request_with(&[("X_Forwarded_For", "1.2.3.4")]);
        run(FieldNameFilter {
            policy: UnsafeFieldNames::Drop,
            superglobals: false,
        }
        .request_header_filter(&mut header))
        .unwrap();
        assert!(header.headers.get("x_forwarded_for").is_some());
    }

    #[test]
    fn only_alphanumerics_and_dash_are_safe_field_names() {
        assert!(is_safe_field_name("x-forwarded-for"));
        assert!(is_safe_field_name("Sec-Ch-Ua-Mobile"));
        // Aliases today: `-` becomes `_`, and PHP rewrites `.` to `_` as well.
        assert!(!is_safe_field_name("x_forwarded_for"));
        assert!(!is_safe_field_name("x.forwarded.for"));
        // Legal tchars that do not alias today; the allowlist covers them anyway.
        assert!(!is_safe_field_name("x~foo"));
        assert!(!is_safe_field_name("x$foo"));
    }

    #[test]
    fn drop_removes_only_the_unsafe_names() {
        let names = surviving(
            UnsafeFieldNames::Drop,
            &[
                ("X-Forwarded-For", "203.0.113.7"),
                ("X_Forwarded_For", "1.2.3.4"),
                ("X.Forwarded.For", "5.6.7.8"),
            ],
        )
        .unwrap();
        assert_eq!(names, ["x-forwarded-for"]);
    }

    #[test]
    fn reject_answers_400_only_when_a_name_is_unsafe() {
        let err =
            surviving(UnsafeFieldNames::Reject, &[("X_Forwarded_For", "1.2.3.4")]).unwrap_err();
        assert_eq!(err.etype(), &ErrorType::HTTPStatus(400));
        let names = surviving(UnsafeFieldNames::Reject, &[("X-Forwarded-For", "1.2.3.4")]).unwrap();
        assert_eq!(names, ["x-forwarded-for"]);
    }

    #[test]
    fn hop_by_hop_names_match_case_insensitively() {
        assert!(skip_response_header("Transfer-Encoding"));
        assert!(skip_response_header("PROXY-CONNECTION"));
        assert!(!skip_response_header("content-type"));
    }
}
