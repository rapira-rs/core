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

pub(crate) struct InflightGuard {
    counter: Arc<AtomicUsize>,
}

impl InflightGuard {
    fn arm(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self {
            counter: Arc::clone(counter),
        }
    }
}

impl Drop for InflightGuard {
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
    Empty,
    /// A body that did not reach PHP: a front-authored refusal or a middleware answer.
    /// The guard keeps the drain window open until hyper finishes the write.
    Guarded {
        body: extension_api::Body,
        _guard: Option<InflightGuard>,
    },
}

fn refused(status: http::StatusCode, guard: Option<InflightGuard>) -> http::Response<RespBody> {
    error_response(status).map(|body| RespBody::Guarded {
        body,
        _guard: guard,
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
            RespBody::Empty => Poll::Ready(None),
            RespBody::Guarded { body: b, .. } => Pin::new(b).poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            RespBody::Reply(_) => false,
            RespBody::Empty => true,
            RespBody::Guarded { body: b, .. } => b.is_end_stream(),
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match self {
            RespBody::Reply(b) => b.size_hint(),
            RespBody::Empty => http_body::SizeHint::with_exact(0),
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
    let guard: InflightGuard = InflightGuard::arm(&shared.inflight);
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
            return refused(rej.status, Some(guard));
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
            Some(guard),
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
        guard: Mutex::new(Some(guard)),
    });
    let keep = Arc::clone(&handler);
    let res = Next::new(Arc::clone(&shared.chain), handler).run(req).await;
    // A middleware that answered without the handler left the guard in its slot.
    // Attach it to the body so shutdown counts the response until hyper writes it.
    let guard = keep.guard.lock().expect("guard mutex poisoned").take();
    res.map(|body| RespBody::Guarded {
        body,
        _guard: guard,
    })
}

struct PhpHandler {
    shared: Arc<Shared>,
    closed: tokio::sync::watch::Receiver<bool>,
    authority: Mutex<Option<Option<Vec<u8>>>>,
    guard: Mutex<Option<InflightGuard>>,
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
        let guard = self.guard.lock().expect("guard mutex poisoned").take();
        serve_php(
            &self.shared,
            &self.closed,
            authority,
            guard,
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
    guard: Option<InflightGuard>,
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
        bridge::spawn_drain(reply, closed.clone(), guard);
        RespBody::Empty
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
    use extension_api::{Backend, Reply, Request};
    use std::future::Future;

    struct NoPhp;

    impl Backend for NoPhp {
        fn exec(
            &self,
            _req: Request,
        ) -> Pin<Box<dyn Future<Output = extension_api::Result<Reply>> + Send + '_>> {
            unreachable!("the middleware answers before PHP")
        }
    }

    struct Deny;

    impl Middleware for Deny {
        fn handle<'a>(&'a self, _req: HttpRequest, _next: Next) -> BoxFuture<'a, HttpResponse> {
            Box::pin(async { error_response(http::StatusCode::FORBIDDEN) })
        }
    }

    /// A middleware answer must hold the inflight guard until hyper drops the body.
    #[tokio::test]
    async fn short_circuit_keeps_the_inflight_guard() {
        let inflight: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(Shared {
            cfg: Arc::new(Config::default()),
            php: Php::new(Arc::new(NoPhp)),
            chain: Arc::from(vec![Arc::new(Deny) as Arc<dyn Middleware>]),
            inflight: Arc::clone(&inflight),
        });
        let (_closed_tx, closed) = tokio::sync::watch::channel(false);
        let conn = ConnInfo {
            remote: Addr::Inet(([127, 0, 0, 1], 40000).into()),
            server: Addr::Inet(([127, 0, 0, 1], 8000).into()),
            closed,
        };
        let req = http::Request::builder()
            .uri("/")
            .header("host", "e2e")
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        let res = handle(shared, conn, req).await;
        assert_eq!(res.status(), http::StatusCode::FORBIDDEN);
        assert_eq!(
            inflight.load(Ordering::Acquire),
            1,
            "the guard must ride the response body"
        );
        drop(res);
        assert_eq!(inflight.load(Ordering::Acquire), 0);
    }
}
