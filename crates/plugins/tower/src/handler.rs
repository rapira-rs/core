use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use extension_api::{
    Addr, BoxError, BoxFuture, Handler, HttpRequest, HttpResponse, Middleware, Next, Peer, Php,
    Protocol, Rejected, ReplyEvent,
};
use http_body::Body;
use http_body_util::BodyExt;

use crate::response::{error_response, response_headers};
use crate::{Config, bridge, check, request};

pub(crate) struct Shared {
    pub cfg: Arc<Config>,
    pub php: Php,
    pub chain: Arc<[Arc<dyn Middleware>]>,
    pub inflight: Arc<AtomicUsize>,
}

pub(crate) struct InflightReqCount {
    counter: Arc<AtomicUsize>,
}

impl InflightReqCount {
    fn init(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self {
            counter: Arc::clone(counter),
        }
    }
}

impl Drop for InflightReqCount {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub(crate) struct ConnInfo {
    pub remote: Addr,
    pub server: Addr,
    pub closed: tokio::sync::watch::Receiver<bool>,
}

pub(crate) enum RespBody {
    Reply(bridge::ReplyBody),
    /// The head of a bodiless reply. The guard keeps the drain window open
    /// until hyper writes the head.
    Empty {
        _guard: Option<Arc<InflightReqCount>>,
    },
    /// A body that did not reach PHP: a front-authored refusal or a middleware answer.
    /// The guard keeps the drain window open until hyper finishes the write.
    Guarded {
        body: extension_api::Body,
        _req_count: Option<Arc<InflightReqCount>>,
    },
}

fn refused(
    status: http::StatusCode,
    req_count: Option<Arc<InflightReqCount>>,
) -> http::Response<RespBody> {
    error_response(status).map(|body| RespBody::Guarded {
        body,
        _req_count: req_count,
    })
}

impl Body for RespBody {
    type Data = bytes::Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<bytes::Bytes>, BoxError>>> {
        match self.get_mut() {
            RespBody::Reply(b) => Pin::new(b).poll_frame(cx),
            RespBody::Empty { .. } => Poll::Ready(None),
            RespBody::Guarded { body: b, .. } => Pin::new(b).poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            RespBody::Reply(_) => false,
            RespBody::Empty { .. } => true,
            RespBody::Guarded { body: b, .. } => b.is_end_stream(),
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match self {
            RespBody::Reply(b) => b.size_hint(),
            RespBody::Empty { .. } => http_body::SizeHint::with_exact(0),
            RespBody::Guarded { body: b, .. } => b.size_hint(),
        }
    }
}

pub(crate) struct RapiraService {
    shared: Arc<Shared>,
    conn: ConnInfo,
}

impl RapiraService {
    pub(crate) fn new(shared: Arc<Shared>, conn: ConnInfo) -> Self {
        Self { shared, conn }
    }
}

impl hyper::service::Service<http::Request<hyper::body::Incoming>> for RapiraService {
    type Response = http::Response<RespBody>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<http::Response<RespBody>, Infallible>>;

    fn call(&self, req: http::Request<hyper::body::Incoming>) -> Self::Future {
        let shared = Arc::clone(&self.shared);
        let conn = self.conn.clone();
        Box::pin(async move { Ok(handle(shared, conn, req).await) })
    }
}

async fn handle<B>(
    shared: Arc<Shared>,
    conn: ConnInfo,
    req: http::Request<B>,
) -> http::Response<RespBody>
where
    B: Body<Data = bytes::Bytes> + Unpin + Send + 'static,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let reqs_counter: Arc<InflightReqCount> = Arc::new(InflightReqCount::init(&shared.inflight));
    let received_at: f64 = std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let (mut parts, incoming) = req.into_parts();

    let authority = match check::check_request(
        &mut parts,
        shared.cfg.unsafe_field_names,
        shared.cfg.superglobals,
        shared.cfg.max_body_size,
    ) {
        Ok(authority) => authority,
        Err(rej) => {
            tracing::warn!(target: "http", "rejected: {}", rej.reason);
            return refused(rej.status, Some(reqs_counter));
        }
    };

    let peer: Peer = Peer {
        remote: conn.remote,
        server: conn.server,
        https: false,
        received_at,
    };

    // future middleware are here -----------
    // TODO: so/dylib vs code?
    if shared.chain.is_empty() {
        return serve_php(
            &shared,
            &conn.closed,
            authority,
            Some(reqs_counter),
            &parts,
            incoming,
            &peer,
        )
        .await;
    }

    parts.extensions.insert(Protocol::Http);
    parts.extensions.insert(peer);
    let body: extension_api::Body = incoming.map_err(BoxError::from).boxed_unsync();
    let req = HttpRequest::from_parts(parts, body);

    let handler = Arc::new(PhpHandler {
        shared: Arc::clone(&shared),
        closed: conn.closed,
        authority: Mutex::new(Some(authority)),
        guard: Arc::clone(&reqs_counter),
    });
    let res = Next::new(Arc::clone(&shared.chain), handler).run(req).await;
    // The final response and the PHP reply share one guard; the drain window
    // stays open until the last holder drops.
    res.map(|body| RespBody::Guarded {
        body,
        _req_count: Some(reqs_counter),
    })
}

struct PhpHandler {
    shared: Arc<Shared>,
    closed: tokio::sync::watch::Receiver<bool>,
    authority: Mutex<Option<Option<Vec<u8>>>>,
    guard: Arc<InflightReqCount>,
}

impl Handler for PhpHandler {
    fn call(&self, req: HttpRequest) -> BoxFuture<'_, HttpResponse> {
        Box::pin(self.serve(req))
    }
}

impl PhpHandler {
    async fn serve(&self, req: HttpRequest) -> HttpResponse {
        let (parts, body) = req.into_parts();
        let Some(peer) = parts.extensions.get::<Peer>().cloned() else {
            tracing::error!(target: "http", "peer info missing from request extensions");
            return error_response(http::StatusCode::INTERNAL_SERVER_ERROR);
        };
        let authority = self
            .authority
            .lock()
            .expect("authority mutex poisoned")
            .take()
            .flatten();
        serve_php(
            &self.shared,
            &self.closed,
            authority,
            Some(Arc::clone(&self.guard)),
            &parts,
            body,
            &peer,
        )
        .await
        .map(BodyExt::boxed_unsync)
    }
}

async fn serve_php<B>(
    shared: &Shared,
    closed: &tokio::sync::watch::Receiver<bool>,
    authority: Option<Vec<u8>>,
    guard: Option<Arc<InflightReqCount>>,
    parts: &http::request::Parts,
    body: B,
    peer: &Peer,
) -> http::Response<RespBody>
where
    B: Body<Data = bytes::Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let cfg = &shared.cfg;
    let mut body = body;
    let mut collected: Vec<u8> = Vec::new();
    loop {
        // hyper only times the head read, so each body frame gets its own progress bound here.
        let frame = match tokio::time::timeout(cfg.keepalive_timeout, body.frame()).await {
            Ok(frame) => frame,
            Err(_) => {
                tracing::debug!(target: "http", "request body stalled past keepalive_timeout");
                return refused(http::StatusCode::REQUEST_TIMEOUT, guard);
            }
        };
        match frame {
            None => break,
            Some(Ok(frame)) => {
                // Non-data frames (request trailers) are dropped: PHP has no surface for them.
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                if collected.len() + data.len() > cfg.max_body_size {
                    tracing::warn!(target: "http", "request body exceeds max_body_size");
                    return refused(http::StatusCode::PAYLOAD_TOO_LARGE, guard);
                }
                collected.extend_from_slice(&data);
            }
            Some(Err(e)) => {
                tracing::debug!(target: "http", "request body read failed: {e}");
                return refused(http::StatusCode::BAD_REQUEST, guard);
            }
        }
    }

    let request = request::build(parts, authority, collected, peer, cfg);
    let mut reply = match shared.php.exec(request).await {
        Ok(reply) => reply,
        Err(e) => {
            if let Some(r) = e.downcast_ref::<Rejected>() {
                tracing::warn!(target: "http", "rejected before dispatch: {r}");
                let status = http::StatusCode::from_u16(r.status)
                    .unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR);
                return refused(status, guard);
            }
            let status = if e.chain().any(|c| c.is::<std::io::Error>()) {
                http::StatusCode::INTERNAL_SERVER_ERROR
            } else {
                http::StatusCode::BAD_GATEWAY
            };
            tracing::error!(target: "http", "php exec failed: {e:#}");
            return refused(status, guard);
        }
    };

    let (status, headers, content_length, bodiless) = loop {
        match reply.next().await {
            None => {
                tracing::error!(target: "http", "php worker died before a response head");
                return refused(http::StatusCode::BAD_GATEWAY, guard);
            }
            Some(ReplyEvent::Interim { status, .. }) => {
                tracing::debug!(target: "http", "dropped interim {status}");
            }
            Some(ReplyEvent::Head {
                status,
                headers,
                content_length,
                bodiless,
                ..
            }) => break (status, headers, content_length, bodiless),
            Some(ReplyEvent::End { .. }) => {
                tracing::error!(target: "http", "php produced no response head");
                return refused(http::StatusCode::BAD_GATEWAY, guard);
            }
            Some(ReplyEvent::Chunk(_) | ReplyEvent::File { .. }) => {
                tracing::warn!(target: "http", "dropped body bytes preceding the response head");
            }
        }
    };

    let status = match http::StatusCode::from_u16(status) {
        Ok(s) if s.as_u16() >= 200 => s,
        _ => {
            // hyper reacts to a service-supplied 1xx by rewriting it to 500 and erroring
            // the connection; a 502 head keeps the connection coherent.
            tracing::error!(
                target: "http",
                "php committed status {status} as final; this front cannot forward it - serving 502"
            );
            http::StatusCode::BAD_GATEWAY
        }
    };

    let no_body =
        bodiless || matches!(status.as_u16(), 204 | 304) || parts.method == http::Method::HEAD;
    let declared_cl = content_length.filter(|_| !no_body);

    let body: RespBody = if no_body {
        bridge::spawn_drain(reply, closed.clone(), guard.clone());
        RespBody::Empty { _guard: guard }
    } else {
        let staged = if declared_cl.is_some() {
            tokio::time::timeout(Duration::from_millis(10), reply.next())
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        RespBody::Reply(bridge::ReplyBody::new(reply, declared_cl, guard, staged))
    };

    let mut res = http::Response::new(body);
    *res.status_mut() = status;
    *res.headers_mut() = response_headers(headers, declared_cl, no_body);
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use extension_api::{Backend, Reply, ReplySource, Request};
    use std::future::Future;
    use std::sync::atomic::AtomicBool;

    struct NoPhp;

    impl Backend for NoPhp {
        fn exec(
            &self,
            _req: Request,
        ) -> Pin<Box<dyn Future<Output = extension_api::Result<Reply>> + Send + '_>> {
            unreachable!("the middleware answers before PHP")
        }
    }

    struct TestSource {
        events: Vec<ReplyEvent>,
        dropped: Option<Arc<AtomicBool>>,
    }

    impl ReplySource for TestSource {
        fn poll_next(&mut self, _cx: &mut Context<'_>) -> Poll<Option<ReplyEvent>> {
            match self.events.is_empty() {
                true => Poll::Ready(None),
                false => Poll::Ready(Some(self.events.remove(0))),
            }
        }
    }

    impl Drop for TestSource {
        fn drop(&mut self) {
            if let Some(flag) = &self.dropped {
                flag.store(true, Ordering::Release);
            }
        }
    }

    struct Scripted {
        events: Mutex<Option<Vec<ReplyEvent>>>,
        dropped: Option<Arc<AtomicBool>>,
    }

    impl Backend for Scripted {
        fn exec(
            &self,
            _req: Request,
        ) -> Pin<Box<dyn Future<Output = extension_api::Result<Reply>> + Send + '_>> {
            let events = self
                .events
                .lock()
                .unwrap()
                .take()
                .expect("one exec per test");
            let dropped = self.dropped.clone();
            Box::pin(async move { Ok(Reply::new(Box::new(TestSource { events, dropped }))) })
        }
    }

    fn head(bodiless: bool) -> ReplyEvent {
        ReplyEvent::Head {
            status: 200,
            headers: Vec::new(),
            content_length: None,
            bodiless,
            body_coded: false,
        }
    }

    fn end() -> ReplyEvent {
        ReplyEvent::End {
            trailers: Vec::new(),
            truncated: false,
        }
    }

    struct Deny;

    impl Middleware for Deny {
        fn handle<'a>(&'a self, _req: HttpRequest, _next: Next) -> BoxFuture<'a, HttpResponse> {
            Box::pin(async { error_response(http::StatusCode::FORBIDDEN) })
        }
    }

    struct Replace;

    impl Middleware for Replace {
        fn handle<'a>(&'a self, req: HttpRequest, next: Next) -> BoxFuture<'a, HttpResponse> {
            Box::pin(async move {
                let _ = next.run(req).await;
                error_response(http::StatusCode::IM_A_TEAPOT)
            })
        }
    }

    struct Pass;

    impl Middleware for Pass {
        fn handle<'a>(&'a self, req: HttpRequest, next: Next) -> BoxFuture<'a, HttpResponse> {
            Box::pin(async move { next.run(req).await })
        }
    }

    #[allow(clippy::type_complexity)]
    fn setup(
        backend: Arc<dyn Backend>,
        chain: Vec<Arc<dyn Middleware>>,
    ) -> (
        Arc<Shared>,
        ConnInfo,
        Arc<AtomicUsize>,
        tokio::sync::watch::Sender<bool>,
    ) {
        let inflight: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(Shared {
            cfg: Arc::new(Config::default()),
            php: Php::new(backend),
            chain: chain.into(),
            inflight: Arc::clone(&inflight),
        });
        let (closed_tx, closed) = tokio::sync::watch::channel(false);
        let conn = ConnInfo {
            remote: Addr::Inet(([127, 0, 0, 1], 40000).into()),
            server: Addr::Inet(([127, 0, 0, 1], 8000).into()),
            closed,
        };
        (shared, conn, inflight, closed_tx)
    }

    fn get_request() -> http::Request<http_body_util::Empty<bytes::Bytes>> {
        http::Request::builder()
            .uri("/")
            .header("host", "e2e")
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap()
    }

    /// A middleware answer must hold the inflight guard until hyper drops the body.
    #[tokio::test]
    async fn short_circuit_keeps_the_inflight_guard() {
        let (shared, conn, inflight, _closed_tx) =
            setup(Arc::new(NoPhp), vec![Arc::new(Deny) as Arc<dyn Middleware>]);
        let res = handle(shared, conn, get_request()).await;
        assert_eq!(res.status(), http::StatusCode::FORBIDDEN);
        assert_eq!(
            inflight.load(Ordering::Acquire),
            1,
            "the guard must ride the response body"
        );
        drop(res);
        assert_eq!(inflight.load(Ordering::Acquire), 0);
    }

    /// A middleware that replaces the PHP response must keep the request counted
    /// until hyper drops the replacement body.
    #[tokio::test]
    async fn replaced_response_keeps_the_inflight_guard() {
        let backend = Arc::new(Scripted {
            events: Mutex::new(Some(vec![head(false), end()])),
            dropped: None,
        });
        let (shared, conn, inflight, _closed_tx) =
            setup(backend, vec![Arc::new(Replace) as Arc<dyn Middleware>]);
        let res = handle(shared, conn, get_request()).await;
        assert_eq!(res.status(), http::StatusCode::IM_A_TEAPOT);
        assert_eq!(
            inflight.load(Ordering::Acquire),
            1,
            "the guard must ride the replacement response"
        );
        drop(res);
        assert_eq!(inflight.load(Ordering::Acquire), 0);
    }

    /// One request counts once, no matter how many holders share the guard.
    #[tokio::test]
    async fn chained_response_counts_one_request() {
        let backend = Arc::new(Scripted {
            events: Mutex::new(Some(vec![head(false), end()])),
            dropped: None,
        });
        let (shared, conn, inflight, _closed_tx) =
            setup(backend, vec![Arc::new(Pass) as Arc<dyn Middleware>]);
        let res = handle(shared, conn, get_request()).await;
        assert_eq!(res.status(), http::StatusCode::OK);
        assert_eq!(
            inflight.load(Ordering::Acquire),
            1,
            "one request must count once"
        );
        drop(res);
        assert_eq!(inflight.load(Ordering::Acquire), 0);
    }

    /// A bodiless reply keeps the response guarded after the drain task finishes.
    #[tokio::test]
    async fn bodiless_response_stays_guarded_after_the_drain_ends() {
        let dropped = Arc::new(AtomicBool::new(false));
        let backend = Arc::new(Scripted {
            events: Mutex::new(Some(vec![head(true), end()])),
            dropped: Some(Arc::clone(&dropped)),
        });
        let (shared, conn, inflight, _closed_tx) = setup(backend, Vec::new());
        let res = handle(shared, conn, get_request()).await;
        assert_eq!(res.status(), http::StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dropped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drain must run to End");
        assert_eq!(
            inflight.load(Ordering::Acquire),
            1,
            "the guard must ride the empty response"
        );
        drop(res);
        assert_eq!(inflight.load(Ordering::Acquire), 0);
    }
}
