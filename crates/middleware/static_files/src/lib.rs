use std::path::PathBuf;

use bytes::Bytes;
use extension_api::{BoxError, BoxFuture, HttpRequest, HttpResponse, Middleware, Next};
use http::{Method, StatusCode};
use http_body_util::{BodyExt, Empty};
use tower_http::services::ServeDir;

/// Serves files from a directory and hands every miss to the next middleware.
pub struct StaticFiles {
    dir: ServeDir,
    forbid: Vec<String>,
}

impl StaticFiles {
    /// `root` must be absolute; `forbid` holds lowercase extensions with a leading dot.
    pub fn new(root: PathBuf, forbid: Vec<String>) -> Self {
        Self {
            dir: ServeDir::new(root),
            forbid,
        }
    }

    /// The check runs on the decoded path because ServeDir percent-decodes before it touches
    /// the filesystem; matching the raw path would let `%2Ephp` through.
    fn eligible(&self, path: &str) -> bool {
        let Ok(decoded) = percent_encoding::percent_decode_str(path).decode_utf8() else {
            return false;
        };
        if decoded.split('/').any(|segment| segment.starts_with('.')) {
            return false;
        }
        // file_name() drops trailing separators the way ServeDir's component walk does, so
        // `/index.php%2F` still names index.php here.
        let file = std::path::Path::new(decoded.as_ref())
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or_default();
        let file = file.to_ascii_lowercase();
        !self.forbid.iter().any(|ext| file.ends_with(ext.as_str()))
    }
}

impl Middleware for StaticFiles {
    fn handle<'a>(&'a self, req: HttpRequest, next: Next) -> BoxFuture<'a, HttpResponse> {
        Box::pin(async move {
            if req.method() != Method::GET && req.method() != Method::HEAD {
                return next.run(req).await;
            }
            if !self.eligible(req.uri().path()) {
                return next.run(req).await;
            }

            // The probe carries only the head; the original request stays intact for the
            // miss path, so the Peer and Protocol extensions reach the handler untouched.
            let mut probe = http::Request::new(Empty::<Bytes>::new());
            *probe.method_mut() = req.method().clone();
            *probe.uri_mut() = req.uri().clone();
            *probe.headers_mut() = req.headers().clone();

            let mut dir = self.dir.clone();
            match dir.try_call(probe).await {
                // A 307 names a directory, not a servable file; it falls through so PHP owns
                // the URL shape.
                Ok(res)
                    if res.status() != StatusCode::NOT_FOUND
                        && res.status() != StatusCode::TEMPORARY_REDIRECT =>
                {
                    res.map(|b| b.map_err(|e| -> BoxError { Box::new(e) }).boxed_unsync())
                }
                Ok(_) => next.run(req).await,
                // A name-shaped error (segment over NAME_MAX, NUL byte) is a miss, not a failure.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::InvalidFilename | std::io::ErrorKind::InvalidInput
                    ) =>
                {
                    next.run(req).await
                }
                // try_call answers Ok(404) for a missing or unreadable file, so an Err here is
                // a real read failure and must not reach PHP. https://docs.rs/tower-http/0.7.0/tower_http/services/struct.ServeDir.html#method.try_call
                Err(e) => {
                    tracing::error!(target: "http", "static probe failed for {}: {e}", req.uri().path());
                    http::Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Empty::new().map_err(|e| match e {}).boxed_unsync())
                        .unwrap()
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use extension_api::{Addr, BoxFuture, Handler, Middleware, Peer, Protocol};
    use http_body_util::{BodyExt, Full};
    use std::net::SocketAddr;
    use std::sync::Arc;

    fn peer() -> Peer {
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        Peer {
            remote: Addr::Inet(addr),
            server: Addr::Inet(addr),
            https: false,
            received_at: 0.0,
        }
    }

    /// Marks fallthrough: the response reports whether the extensions and body survived.
    struct Fallthrough;

    impl Handler for Fallthrough {
        fn call<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, HttpResponse> {
            Box::pin(async move {
                let kept = req.extensions().get::<Peer>().is_some()
                    && req.extensions().get::<Protocol>().is_some();
                let body = req.into_body().collect().await.unwrap().to_bytes();
                http::Response::builder()
                    .status(200)
                    .header("x-handler", "php")
                    .header("x-extensions", if kept { "kept" } else { "lost" })
                    .body(Full::new(body).map_err(|e| match e {}).boxed_unsync())
                    .unwrap()
            })
        }
    }

    fn request(method: &str, path: &str, body: &str) -> HttpRequest {
        let mut req = http::Request::builder()
            .method(method)
            .uri(path)
            .body(
                Full::new(Bytes::from(body.to_owned()))
                    .map_err(|e| match e {})
                    .boxed_unsync(),
            )
            .unwrap();
        req.extensions_mut().insert(Protocol::Http);
        req.extensions_mut().insert(peer());
        req
    }

    async fn run(st: StaticFiles, req: HttpRequest) -> HttpResponse {
        let chain: Arc<[Arc<dyn Middleware>]> =
            Arc::from(vec![Arc::new(st) as Arc<dyn Middleware>]);
        Next::new(chain, Arc::new(Fallthrough)).run(req).await
    }

    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("styles.css"), "body{}").unwrap();
        std::fs::write(dir.path().join("data.bin"), "abcdefghij").unwrap();
        std::fs::write(dir.path().join("index.html"), "<h1>hi</h1>").unwrap();
        std::fs::write(dir.path().join("index.php"), "<?php secret();").unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=1").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("config"), "[core]").unwrap();
        std::fs::write(dir.path().join("Upper.PHP"), "<?php upper();").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("index.php"), "<?php sub();").unwrap();
        std::fs::create_dir(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets").join("a.css"), "a{}").unwrap();
        dir
    }

    fn static_files(dir: &tempfile::TempDir) -> StaticFiles {
        StaticFiles::new(dir.path().to_path_buf(), vec![".php".to_owned()])
    }

    fn header<'r>(res: &'r HttpResponse, name: &str) -> &'r str {
        res.headers()
            .get(name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .to_str()
            .unwrap()
    }

    async fn body(res: HttpResponse) -> Bytes {
        res.into_body().collect().await.unwrap().to_bytes()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn serves_a_file_with_type_length_and_validators() {
        let dir = root();
        let res = run(static_files(&dir), request("GET", "/styles.css", "")).await;
        assert_eq!(res.status(), 200);
        assert!(res.headers().get("x-handler").is_none());
        assert_eq!(header(&res, "content-type"), "text/css");
        assert_eq!(header(&res, "content-length"), "6");
        assert_eq!(header(&res, "accept-ranges"), "bytes");
        assert!(res.headers().contains_key("etag"));
        assert!(res.headers().contains_key("last-modified"));
        assert_eq!(body(res).await, "body{}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_miss_falls_through_with_the_request_intact() {
        let dir = root();
        let res = run(static_files(&dir), request("GET", "/nope.txt", "ping")).await;
        assert_eq!(header(&res, "x-handler"), "php");
        assert_eq!(header(&res, "x-extensions"), "kept");
        assert_eq!(body(res).await, "ping");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_get_head_methods_fall_through() {
        let dir = root();
        let res = run(static_files(&dir), request("POST", "/styles.css", "p")).await;
        assert_eq!(header(&res, "x-handler"), "php");
        assert_eq!(body(res).await, "p");
    }

    /// The encoded-slash forms decode to a trailing separator that ServeDir drops when it resolves the file.
    #[tokio::test(flavor = "current_thread")]
    async fn forbidden_extensions_fall_through_even_when_the_file_exists() {
        let dir = root();
        for path in [
            "/index.php",
            "/INDEX.PHP",
            "/index%2Ephp",
            "/Upper.PHP",
            "/index.php%2F",
            "/index.php%2F%2F",
            "/sub/index.php%2F",
        ] {
            let res = run(static_files(&dir), request("GET", path, "")).await;
            assert_eq!(header(&res, "x-handler"), "php", "{path}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn directory_paths_without_a_trailing_slash_fall_through() {
        let dir = root();
        for path in ["/assets", "/sub"] {
            let res = run(static_files(&dir), request("GET", path, "")).await;
            assert_eq!(header(&res, "x-handler"), "php", "{path}");
        }

        let res = run(static_files(&dir), request("GET", "/assets/a.css", "")).await;
        assert_eq!(res.status(), 200);
        assert_eq!(body(res).await, "a{}");
    }

    /// A segment over NAME_MAX surfaces from try_call as an Err with a name-shaped kind, not as a 404.
    #[tokio::test(flavor = "current_thread")]
    async fn overlong_and_undecodable_paths_fall_through() {
        let dir = root();
        let long = format!("/{}", "a".repeat(300));
        for path in [long.as_str(), "/%FF.css"] {
            let res = run(static_files(&dir), request("GET", path, "")).await;
            assert_eq!(header(&res, "x-handler"), "php", "{path}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dotfile_segments_fall_through() {
        let dir = root();
        for path in ["/.env", "/.git/config", "/%2Eenv"] {
            let res = run(static_files(&dir), request("GET", path, "")).await;
            assert_eq!(header(&res, "x-handler"), "php", "{path}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn traversal_attempts_fall_through() {
        let dir = root();
        for path in ["/../outside.txt", "/%2e%2e/outside.txt"] {
            let res = run(static_files(&dir), request("GET", path, "")).await;
            assert_eq!(header(&res, "x-handler"), "php", "{path}");
        }
    }

    /// Byte positions are inclusive (RFC 9110 section 14.1.2, https://www.rfc-editor.org/rfc/rfc9110#section-14.1.2); Content-Range carries first-pos "-" last-pos "/" complete-length (section 14.4, https://www.rfc-editor.org/rfc/rfc9110#section-14.4).
    #[tokio::test(flavor = "current_thread")]
    async fn a_range_request_answers_the_named_bytes() {
        let dir = root();
        let mut req = request("GET", "/data.bin", "");
        req.headers_mut()
            .insert("range", "bytes=0-4".parse().unwrap());
        let res = run(static_files(&dir), req).await;
        assert_eq!(res.status(), 206);
        assert_eq!(header(&res, "content-range"), "bytes 0-4/10");
        assert_eq!(header(&res, "content-length"), "5");
        assert_eq!(body(res).await, "abcde");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn if_none_match_with_the_served_etag_answers_304() {
        let dir = root();
        let first = run(static_files(&dir), request("GET", "/data.bin", "")).await;
        let etag = header(&first, "etag").to_owned();

        let mut req = request("GET", "/data.bin", "");
        req.headers_mut()
            .insert("if-none-match", etag.parse().unwrap());
        let res = run(static_files(&dir), req).await;
        assert_eq!(res.status(), 304);
        assert_eq!(body(res).await, "");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn head_answers_the_full_length_without_a_body() {
        let dir = root();
        let res = run(static_files(&dir), request("HEAD", "/data.bin", "")).await;
        assert_eq!(res.status(), 200);
        assert_eq!(header(&res, "content-length"), "10");
        assert_eq!(body(res).await, "");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_root_serves_index_html() {
        let dir = root();
        let res = run(static_files(&dir), request("GET", "/", "")).await;
        assert_eq!(res.status(), 200);
        assert_eq!(header(&res, "content-type"), "text/html");
        assert_eq!(body(res).await, "<h1>hi</h1>");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn query_strings_do_not_affect_resolution() {
        let dir = root();
        let res = run(static_files(&dir), request("GET", "/styles.css?v=2", "")).await;
        assert_eq!(res.status(), 200);
        assert_eq!(body(res).await, "body{}");
    }
}
