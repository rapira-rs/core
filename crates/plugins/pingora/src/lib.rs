use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use extension_api::{
    Extension, FieldLines, ListenAddr, Php, PrepareCtx, PreparedListener, ReplyEvent, Request,
    Result,
};
use pingora::http::{Method, RequestHeader, ResponseHeader, Version};
use pingora::modules::http::compression::ResponseCompressionBuilder;
use pingora::modules::http::{HttpModule, HttpModuleBuilder, HttpModules, Module};
use pingora::proxy::{ProxyHttp, Session, http_proxy_service};
use pingora::server::configuration::ServerConf;
use pingora::server::{Fds, ListenFds};
use pingora::services::Service as _;
use pingora::upstreams::peer::HttpPeer;
use pingora::{Error, ErrorType, Result as PingoraResult};
use tokio::runtime::{self, Builder};
use tokio::sync::{oneshot, watch};

#[derive(Clone, Debug)]
pub enum Listen {
    Tcp(std::net::SocketAddr),
    Unix(std::path::PathBuf),
}

#[derive(Clone)]
pub struct Config {
    pub listen: Listen,
    pub server_name: String,
    pub server_port: u16,
    pub max_body_size: usize,
    pub unsafe_field_names: UnsafeFieldNames,
    /// Drop protects the $_SERVER mapping only, so it is inert without a pool serving superglobals; Reject applies regardless.
    pub superglobals: bool,
    /// Per-write bound on the response path, not a whole-response deadline.
    pub write_timeout: Duration,
    /// Must expire before the host escalates its stop, or the drain is cut short anyway.
    pub drain_grace: Duration,
}

/// No "allow" arm: a boolean off-switch re-opens the CGI aliasing collision (CVE-2026-52845); an allowlist of expected names could be safe, a boolean cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsafeFieldNames {
    Drop,
    Reject,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: Listen::Tcp(std::net::SocketAddr::from(([127, 0, 0, 1], 8000))),
            server_name: "localhost".to_owned(),
            server_port: 8000,
            max_body_size: 8 * 1024 * 1024,
            unsafe_field_names: UnsafeFieldNames::Drop,
            superglobals: true,
            write_timeout: Duration::from_secs(30),
            drain_grace: Duration::from_secs(25),
        }
    }
}

/// A name carrying `_` or `.` lands on the CGI variable a `-` name owns, so this allowlists instead of denying those bytes.
fn is_safe_field_name(name: &str) -> bool {
    name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Serves as both the module builder and the per-request module: `init` copies two plain fields.
struct FieldNameFilter {
    policy: UnsafeFieldNames,
    superglobals: bool,
}

#[async_trait]
impl HttpModule for FieldNameFilter {
    async fn request_header_filter(&mut self, req: &mut RequestHeader) -> PingoraResult<()> {
        if self.policy == UnsafeFieldNames::Drop && !self.superglobals {
            return Ok(());
        }
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

pub struct HttpServer {
    config: Config,
    /// Master-bound listener carried across the fork; None means self-bind (no prepare phase).
    prepared: Option<PreparedListener>,
    shutdown: Option<watch::Sender<bool>>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl Extension for HttpServer {
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
                let _ = done_tx.send(());
                result
            })?;

        self.shutdown = Some(shutdown_tx);
        self.thread = Some(thread);

        let _ = done_rx.await;

        match self.thread.take() {
            Some(thread) => join_thread(thread),
            None => Ok(()),
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(true);
        }
        if let Some(thread) = self.thread.take() {
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

/// The endpoint string and the Fds key must be one and the same string: pingora adopts an inherited fd only on an exact match, and a mismatch silently rebinds (for unix sockets it unlinks and steals the master's socket).
async fn serve(
    php: Php,
    config: Config,
    prepared: Option<PreparedListener>,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let conf: Arc<ServerConf> = Arc::new(ServerConf::default());
    let inflight: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let listen = config.listen.clone();
    let drain_grace = config.drain_grace;

    let (addr, fds): (ListenAddr, Option<ListenFds>) = match prepared {
        Some(p) => {
            use std::os::fd::IntoRawFd;
            let addr = p.addr().clone();
            let key = p.addr_string()?;
            let mut table = Fds::new();
            table.add(key, p.into_raw_fd());
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
            let s = path
                .to_str()
                .ok_or_else(|| anyhow!("unix socket path must be valid UTF-8"))?;
            service.add_uds(s, None);
            tracing::info!(target: "http", "listening on unix:{s}");
        }
    }
    service.start_service(fds, shutdown, 1).await;
    let deadline = tokio::time::Instant::now() + drain_grace;
    while inflight.load(Ordering::Acquire) > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stranded = inflight.load(Ordering::Acquire);
    if stranded > 0 {
        return Err(anyhow!(
            "http drain timed out after {drain_grace:?} with {stranded} request(s) in flight; \
             their responses were cut short"
        ));
    }
    tracing::info!(target: "http", "drained cleanly; accept loop stopped");
    Ok(())
}

struct PhpProxy {
    php: Php,
    config: Config,
    listen: ListenAddr,
    /// Incremented in `new_ctx`, decremented in `logging`; `serve` drains it on shutdown.
    inflight: Arc<AtomicUsize>,
}

pub struct ReqCtx {
    /// Unix seconds when this host accepted the request.
    received_at: f64,
}

#[async_trait]
impl ProxyHttp for PhpProxy {
    type CTX = ReqCtx;

    fn new_ctx(&self) -> Self::CTX {
        self.inflight.fetch_add(1, Ordering::AcqRel);
        ReqCtx {
            received_at: std::time::UNIX_EPOCH
                .elapsed()
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
        }
    }

    /// An override replaces the trait default's whole body, so compression is re-added here; the field-name module then runs ahead of `request_filter`.
    fn init_downstream_modules(&self, modules: &mut HttpModules) {
        modules.add_module(ResponseCompressionBuilder::enable(0));
        modules.add_module(Box::new(FieldNameFilter {
            policy: self.config.unsafe_field_names,
            superglobals: self.config.superglobals,
        }));
    }

    async fn logging(&self, _session: &mut Session, _e: Option<&Error>, _ctx: &mut Self::CTX) {
        self.inflight.fetch_sub(1, Ordering::AcqRel);
    }

    /// Keepalive is flipped off before the head write: pingora renders the Connection header from `will_keepalive()`.
    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<bool> {
        let Some(request) = build_request(session, &self.config, &self.listen, ctx).await? else {
            return Ok(true);
        };

        let mut reply = match self.php.exec(request).await {
            Ok(reply) => reply,
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

        session.set_write_timeout(Some(self.config.write_timeout));
        let wt = self.config.write_timeout;
        let mut head_seen = false;
        let mut no_body = false;
        let mut declared_cl: Option<u64> = None;
        let mut body_sent: u64 = 0;
        let mut watch_idle = true;

        let truncated = loop {
            let ev = tokio::select! {
                biased;
                ev = reply.next() => ev,
                res = session.read_body_or_idle(true), if watch_idle => {
                    match res {
                        Err(e) if matches!(e.etype(), ErrorType::ConnectionClosed) => {
                            drop(reply);
                            session.set_keepalive(None);
                            return Err(Error::explain(
                                ErrorType::ConnectionClosed,
                                "downstream closed while streaming",
                            )
                            .into_down());
                        }
                        _ => {
                            watch_idle = false;
                            session.set_keepalive(None);
                            continue;
                        }
                    }
                }
            };
            let Some(ev) = ev else {
                if !head_seen {
                    return Err(Error::explain(
                        ErrorType::HTTPStatus(502),
                        "php worker died before a response head",
                    ));
                }
                break true;
            };
            match ev {
                ReplyEvent::Interim { status, headers } => {
                    // HTTP/1.0 defines no 1xx class; interim heads are advisory and drop where the protocol has no room (RFC 8297 §3).
                    // https://www.rfc-editor.org/rfc/rfc8297#section-3
                    if session.req_header().version == Version::HTTP_10 || head_seen {
                        tracing::debug!(target: "http", "dropped interim {status}");
                        continue;
                    }
                    let header = build_interim_header(status, headers)?;
                    timed(wt, session.write_response_header(Box::new(header), false)).await?;
                }
                ReplyEvent::Head {
                    status,
                    headers,
                    content_length,
                    bodiless,
                    ..
                } => {
                    let status = if status < 200 {
                        tracing::error!(
                            target: "http",
                            "php committed status {status} as final; this front cannot forward it - serving 502"
                        );
                        502
                    } else {
                        status
                    };
                    no_body = bodiless
                        || matches!(status, 204 | 304)
                        || session.req_header().method == Method::HEAD;
                    let http11 = session.req_header().version == Version::HTTP_11;
                    let chunked = !no_body && content_length.is_none() && http11;
                    if !no_body && content_length.is_none() && !http11 {
                        session.set_keepalive(None);
                    }
                    declared_cl = content_length.filter(|_| !no_body);
                    let header =
                        build_response_header(status, headers, declared_cl, chunked, no_body)?;
                    timed(wt, session.write_response_header(Box::new(header), false)).await?;
                    head_seen = true;
                }
                ReplyEvent::Chunk(b) => {
                    if no_body || b.is_empty() {
                        continue;
                    }
                    body_sent += b.len() as u64;
                    session.write_response_body(Some(b), false).await?;
                }
                ReplyEvent::File { file, offset, len } => {
                    if no_body {
                        continue;
                    }
                    body_sent += pump_file(session, file, offset, len, wt).await?;
                }
                ReplyEvent::End { .. } if !head_seen => {
                    return Err(Error::explain(
                        ErrorType::HTTPStatus(502),
                        "php produced no response head",
                    ));
                }
                ReplyEvent::End { truncated, .. } => {
                    break truncated || declared_cl.is_some_and(|cl| body_sent < cl);
                }
            }
        };

        if truncated {
            session.set_keepalive(None);
            return Err(
                Error::explain(ErrorType::ConnectionClosed, "php response truncated").into_down(),
            );
        }
        timed(wt, session.write_response_body(None, true)).await?;
        Ok(true)
    }

    fn suppress_error_log(&self, _session: &Session, _ctx: &Self::CTX, error: &Error) -> bool {
        matches!(
            error.etype(),
            ErrorType::ConnectionClosed | ErrorType::WriteError | ErrorType::WriteTimedout
        )
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> PingoraResult<Box<HttpPeer>> {
        Err(Error::explain(
            ErrorType::InternalError,
            "rapira-pingora serves all requests locally; no upstream",
        ))
    }
}

/// The Connection-named removals run before our own framing goes in, so a `Connection: content-length` cannot strip it.
fn build_response_header(
    status: u16,
    headers: FieldLines,
    content_length: Option<u64>,
    chunked: bool,
    no_body: bool,
) -> PingoraResult<ResponseHeader> {
    let mut header = ResponseHeader::build(status, Some(headers.len() + 1))?;
    // Extra hop-by-hop fields named by a Connection value (RFC 9110 §7.6.1).
    // https://www.rfc-editor.org/rfc/rfc9110#section-7.6.1
    let mut conn_named: Vec<String> = Vec::new();
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("connection") {
            connection_named_headers(&value, &mut conn_named);
            continue;
        }
        // Framing and connection management are ours, never PHP's (hop-by-hop, RFC 9110 §7.6.1).
        if skip_response_header(&name) {
            continue;
        }
        let logged = name.clone();
        if let Err(e) = header.append_header(name, value) {
            tracing::debug!(target: "http", "dropped response header {logged}: {e}");
        }
    }
    for tok in &conn_named {
        header.remove_header(tok.as_str());
    }
    // A bodiless response (204/304/HEAD/1xx) carries neither framing field: RFC 9110 §8.6 and RFC 9112 §6.1.
    // https://www.rfc-editor.org/rfc/rfc9112#section-6.1
    if !no_body {
        if let Some(n) = content_length {
            header.insert_header("Content-Length", n.to_string())?;
        } else if chunked {
            header.insert_header("Transfer-Encoding", "chunked")?;
        }
    }
    Ok(header)
}

fn build_interim_header(status: u16, headers: FieldLines) -> PingoraResult<ResponseHeader> {
    let mut header = ResponseHeader::build(status, Some(headers.len()))?;
    for (name, value) in headers {
        if skip_response_header(&name) || name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let logged = name.clone();
        if let Err(e) = header.append_header(name, value) {
            tracing::debug!(target: "http", "dropped interim header {logged}: {e}");
        }
    }
    Ok(header)
}

/// `set_write_timeout` bounds body writes only; head writes and the terminator need this explicit bound.
async fn timed<T>(
    wt: Duration,
    fut: impl std::future::Future<Output = PingoraResult<T>>,
) -> PingoraResult<T> {
    match tokio::time::timeout(wt, fut).await {
        Ok(r) => r,
        Err(_) => {
            Err(Error::explain(ErrorType::WriteTimedout, "response write timed out").into_down())
        }
    }
}

/// Returns the bytes written, short when the file shrank under the validated slice.
async fn pump_file(
    session: &mut Session,
    file: std::fs::File,
    offset: u64,
    len: u64,
    wt: Duration,
) -> PingoraResult<u64> {
    use std::os::unix::fs::FileExt;
    let mut file = file;
    let mut done: u64 = 0;
    while done < len {
        let want = std::cmp::min(64 * 1024, len - done) as usize;
        let off = offset + done;
        let (f, res) = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; want];
            let res = file.read_at(&mut buf, off).map(|n| {
                buf.truncate(n);
                buf
            });
            (file, res)
        })
        .await
        .map_err(|e| Error::explain(ErrorType::InternalError, format!("file read task: {e}")))?;
        file = f;
        let buf = res
            .map_err(|e| Error::explain(ErrorType::InternalError, format!("sendfile read: {e}")))?;
        if buf.is_empty() {
            break;
        }
        done += buf.len() as u64;
        timed(wt, session.write_response_body(Some(buf.into()), false)).await?;
    }
    Ok(done)
}

/// Parse a `Connection` value into the lower-cased field names it lists (RFC 9110 §7.6.1).
pub fn connection_named_headers(value: &[u8], out: &mut Vec<String>) {
    for tok in value.split(|&b| b == b',') {
        let tok = String::from_utf8_lossy(tok).trim().to_ascii_lowercase();
        if !tok.is_empty() {
            out.push(tok);
        }
    }
}

/// Headers this front owns instead of PHP (hop-by-hop, RFC 9110 §7.6.1).
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

/// `case_header_iter` yields nothing without a case map (h2's `From<ReqParts>` sets none), hence the lowercase fallback.
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

/// A repeated, missing or empty Host on HTTP/1.1 is a 400 (RFC 9112 §3.2); None means the target URI reconstructs with an empty authority (RFC 9112 §3.3).
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

/// `None` means the request was rejected here and the response is already written.
async fn build_request(
    session: &mut Session,
    config: &Config,
    listen: &ListenAddr,
    ctx: &ReqCtx,
) -> PingoraResult<Option<Request>> {
    let header: &RequestHeader = session.req_header();
    let method: String = header.method.as_str().to_owned();
    let target: Vec<u8> = header.raw_path().to_vec();
    let uri: String = header.uri.to_string();
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
    // The interim 100 goes out only for HTTP/1.1: a server ignores an HTTP/1.0 request's 100-continue expectation (RFC 9110 §10.1.1).
    // https://www.rfc-editor.org/rfc/rfc9110#section-10.1.1
    let expects_continue = header.version == pingora::http::Version::HTTP_11
        && header
            .headers
            .get("expect")
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"100-continue"));

    if declared_len.is_some_and(|len| len > config.max_body_size) {
        reject_payload_too_large(session).await?;
        return Ok(None);
    }
    if expects_continue {
        session.write_continue_response().await?;
    }

    let remote = match session.client_addr() {
        Some(a) => peer_addr(a),
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
    let server = match session.server_addr() {
        Some(a) => peer_addr(a),
        None => listen_addr(listen),
    };

    let mut body: Vec<u8> = Vec::with_capacity(declared_len.unwrap_or(0));
    while let Some(chunk) = session.read_request_body().await? {
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
        https: false,
        protocol,
        remote,
        server,
        server_name: config.server_name.clone(),
        server_port: config.server_port,
        tls: None,
        received_at: Some(ctx.received_at),
        headers,
        body,
    }))
}

/// Closes the connection: the unread body is still on the wire, so it cannot be reused.
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

    /// The Connection-named removals must run before our own Content-Length goes in, or PHP could strip the framing out from under the body.
    #[test]
    fn connection_value_cannot_strip_framing() {
        let head = build_response_header(
            200,
            hdrs(&[
                ("Connection", "content-length, x-drop"),
                ("X-Drop", "1"),
                ("X-Keep", "2"),
            ]),
            Some(7),
            false,
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
            let head =
                build_response_header(status, hdrs(&[("X-A", "1")]), Some(0), false, true).unwrap();
            assert!(head.headers.get("content-length").is_none(), "{status}");
            assert!(head.headers.get("transfer-encoding").is_none(), "{status}");
        }
    }

    /// A space is not a tchar, so no front can put this name on the wire; dropping the field must not cost the rest of the response.
    #[test]
    fn unrepresentable_header_is_dropped_not_fatal() {
        let head = build_response_header(
            200,
            hdrs(&[("Content Type", "text/html"), ("X-Keep", "2")]),
            Some(3),
            false,
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
            Some(4),
            false,
            false,
        )
        .unwrap();
        assert_eq!(head.headers.get("content-length").unwrap().as_bytes(), b"4");
        assert!(head.headers.get("transfer-encoding").is_none());
    }

    /// The plugin is the sole author of chunked framing and a Connection value cannot strip it.
    #[test]
    fn chunked_is_authored_here_and_protected() {
        let head = build_response_header(
            200,
            hdrs(&[("Connection", "transfer-encoding"), ("X-Keep", "1")]),
            None,
            true,
            false,
        )
        .unwrap();
        assert_eq!(
            head.headers.get("transfer-encoding").unwrap().as_bytes(),
            b"chunked"
        );
        assert!(head.headers.get("content-length").is_none());
    }

    /// An interim head never carries framing fields.
    #[test]
    fn interim_head_carries_no_framing() {
        let head = build_interim_header(
            103,
            hdrs(&[("Link", "</a.css>; rel=preload"), ("Content-Length", "5")]),
        )
        .unwrap();
        assert!(head.headers.get("link").is_some());
        assert!(head.headers.get("content-length").is_none());
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

    /// Without a case map (h2's From<ReqParts> path) the fallback still yields every line, lowercase.
    #[test]
    fn no_case_requests_still_yield_headers() {
        let mut header = RequestHeader::build_no_case("GET", b"/", None).unwrap();
        header.append_header("X-Foo".to_string(), "1").unwrap();
        header.append_header("X-Foo".to_string(), "2").unwrap();
        let headers = collect_headers(&header);
        let foos: Vec<_> = headers.iter().filter(|(n, _)| n == "x-foo").collect();
        assert_eq!(foos.len(), 2);
    }

    /// RFC 9112 §3.2: repeated/missing/empty Host on 1.1 is a 400; a bare 1.0 request named no authority.
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

    /// Drop protects the $_SERVER mapping; a dispatcher pool has none, so "names as received" wins there.
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
        assert!(!is_safe_field_name("x_forwarded_for"));
        assert!(!is_safe_field_name("x.forwarded.for"));
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
