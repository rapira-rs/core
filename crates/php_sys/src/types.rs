use bytes::Bytes;
use std::collections::HashMap;
use std::ffi::CString;
use std::io::Read;
use std::os::raw::c_int;
use std::path::PathBuf;
use tokio::sync::mpsc::Sender;

/// Header/trailer fields: one entry per field line, wire order, names as
/// received, values raw bytes (latin1/binary-safe).
pub type FieldLines = Vec<(String, Vec<u8>)>;

#[derive(Debug, Clone)]
pub enum Mode {
    Classic,
    Worker(PathBuf),
    Dispatcher(PathBuf),
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok = 0,
    Bailout = 1,
    Exit = 2,
    Throw = 3,
}

impl Outcome {
    pub fn from_c(v: c_int) -> Self {
        match v {
            0 => Self::Ok,
            1 => Self::Bailout,
            2 => Self::Exit,
            3 => Self::Throw,
            _ => Self::Bailout,
        }
    }
}

/// One event of a response stream. A well-formed stream is
/// `Interim* Head? (Chunk|File)* End?`: `End` without a `Head` only happens
/// when the producer recorded no head at all, and a channel that closes
/// without `End` means the producer died (truncated when a `Head` was seen).
pub enum Frame {
    /// Advisory interim head (100-199, never 101); forwarded where the
    /// protocol allows, dropped otherwise.
    Interim(ResponseHead),
    /// The final head, at most once per response.
    Head {
        head: ResponseHead,
        /// The framing the consumer applies when Some: a declared
        /// content-length being honoured, or the computed length of a response
        /// that ends on its first body write. None means the consumer chooses
        /// (chunked on HTTP/1.1). Never synthesized for a bodiless response.
        content_length: Option<u64>,
        /// 204 | 304 | a HEAD request | 1xx: no body bytes and no framing
        /// fields go on the wire.
        bodiless: bool,
        /// The head carried content-encoding: the body is already coded.
        body_coded: bool,
    },
    Chunk(Bytes),
    /// A file slice the producer opened and validated; the consumer streams it
    /// and owns the handle.
    File {
        file: std::fs::File,
        offset: u64,
        len: u64,
    },
    /// Terminal, exactly once from a live producer. Trailers ride here; a
    /// consumer that cannot express them drops the section.
    End {
        trailers: FieldLines,
        truncated: bool,
    },
}

pub struct Job {
    pub ctx: Context,
}

/// One endpoint of an accepted connection, as the socket reports it. Mirror of
/// `extension_api::Addr` - php_sys does not depend on extension_api, the runtime
/// mapping is the one bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addr {
    Inet(std::net::SocketAddr),
    /// None is an unnamed endpoint - the usual case for a peer connecting to a
    /// unix listener.
    Unix(Option<PathBuf>),
}

pub struct ClientCertView {
    pub serial: String,
    pub organization: Option<String>,
    pub fingerprint: String,
}

pub struct TlsView {
    pub version: String,
    pub cipher: String,
    pub alpn: Option<String>,
    pub server_name: Option<String>,
    pub cert: Option<ClientCertView>,
}

/// A file part spooled by the host. `unlink` is the one remover - seal calls
/// it at finalize, Drop is the abnormal-path net.
pub struct SpooledFile {
    pub path: PathBuf,
}

impl SpooledFile {
    /// Takes the path, so a Drop after an explicit unlink is a no-op and a
    /// recycled file name is never removed twice.
    pub fn unlink(&mut self) {
        let path = std::mem::take(&mut self.path);
        if path.as_os_str().is_empty() {
            return;
        }
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(target: "rapira", "removing spool file {}: {e}", path.display());
        }
    }
}

impl Drop for SpooledFile {
    fn drop(&mut self) {
        self.unlink();
    }
}

pub struct FormField {
    pub name: Vec<u8>,       // content-disposition name, bytes as received
    pub value: Vec<u8>,      // part body, no decoding
    pub headers: FieldLines, // the part's header section, per-value shape
}

pub struct UploadedFile {
    pub name: Vec<u8>,
    /// Byte-for-byte as sent; empty is a browser submitting an empty file input.
    pub client_filename: Vec<u8>,
    /// content-type value verbatim, parameters included; an empty or OWS-only
    /// value maps to None upstream.
    pub client_media_type: Option<Vec<u8>>,
    pub headers: FieldLines,
    pub file: SpooledFile,
    /// Bytes written to disk. A 64-bit zend_long is assumed at the FFI edge.
    pub size: u64,
}

pub struct MultipartBody {
    pub fields: Vec<FormField>,
    pub files: Vec<UploadedFile>,
}

pub enum Body {
    /// Classic-path bodies and non-multipart dispatcher bodies.
    Raw(Box<dyn Read + Send>),
    /// Dispatcher-path multipart, parsed by the host before enqueue.
    Multipart(MultipartBody),
}

pub struct Request {
    pub method: String,
    pub uri: String,
    /// Raw request-target bytes; None falls back to `uri`'s bytes (empty was
    /// normalized to None at the producer).
    pub target: Option<Vec<u8>>,
    /// The authority the client named, byte-for-byte; None = named none.
    pub authority: Option<Vec<u8>>,
    pub https: bool,
    pub query: String,
    /// Wire/CGI spelling ("HTTP/2.0"); mapped to the contract spelling at the
    /// exchange view.
    pub protocol: String,
    pub remote: Addr,
    /// The accepting socket.
    pub server: Addr,
    /// Configured CGI SERVER_NAME/SERVER_PORT and the $uri synthesis fallback.
    pub server_name: String,
    pub server_port: u16,
    pub script_name: String,
    pub document_root: String,
    pub script_filename: PathBuf,
    /// One entry per field line, wire order per name; values as bytes
    /// (latin1/binary-safe).
    pub headers: FieldLines,
    pub server_vars: Vec<(String, String)>,
    /// First content-type field line, raw bytes.
    pub content_type: Option<Vec<u8>>,
    /// Wire byte count; captured before any body move, never re-derived from
    /// parsed parts. -1 if unknown.
    pub content_length: i64,
    pub body: Body,
    /// Unix seconds; None = not yet stamped (the handler stamps).
    pub received_at: Option<f64>,
    pub tls: Option<TlsView>,
}

pub struct ResponseHead {
    pub status: u16,
    pub headers: FieldLines,
}

fn status_field_code(value: &[u8]) -> Option<u16> {
    let digits: &[u8] = value
        .split(|b| b.is_ascii_whitespace())
        .find(|token| !token.is_empty())?;
    let code: u16 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    (100..=599).contains(&code).then_some(code)
}

/// The $_SERVER-facing materialization of a Request: SAPI CStrings, the folded
/// header table, the rendered address strings. Built once, superglobals modes
/// only. `register_server_variables` registers from borrows into this storage -
/// that frame may not hold owned Rust values (a bailout longjmp over pending
/// drops is UB).
pub struct ReqC {
    pub method: CString,
    pub query: CString,
    pub uri: CString,
    pub script: CString,
    pub ctype: Option<CString>,
    pub cookie: Option<CString>,
    pub authorization: Option<CString>,
    pub env: HashMap<Box<[u8]>, CString>,
    /// One deterministic value per name for the `HTTP_*` mapping (crate::fold).
    pub folded_headers: FieldLines,
    /// Rendered CGI address strings: Inet → ip / port, Unix → "" / "0".
    pub remote_addr: String,
    pub remote_port: String,
    pub server_port: String,
}

fn cgi_addr_strings(addr: &Addr) -> (String, String) {
    match addr {
        Addr::Inet(sa) => (sa.ip().to_string(), sa.port().to_string()),
        // A unix peer has no network address, but REMOTE_ADDR must hold a
        // hostnumber (RFC 3875 §4.1.8); loopback is the conventional stand-in.
        // https://www.rfc-editor.org/rfc/rfc3875#section-4.1.8
        Addr::Unix(_) => ("127.0.0.1".to_owned(), "0".to_owned()),
    }
}

/// A NUL byte cannot cross the CGI boundary; the value degrades to empty, and
/// the warn names the field so the degrade is visible.
fn cgi_cstring(field: &str, bytes: &[u8]) -> CString {
    CString::new(bytes).unwrap_or_else(|_| {
        tracing::warn!(target: "rapira", "{field} carries a NUL byte; registered empty");
        CString::default()
    })
}

impl ReqC {
    pub fn build(r: &Request) -> Self {
        let folded_headers = crate::fold::fold_field_lines(&r.headers);

        // Cookie repeats rejoin on "; ", the cookie-string form php-src's parser
        // expects; the folded table already applied that rule.
        let cookie: Option<CString> = folded_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
            .map(|(_, v)| cgi_cstring("Cookie", v));

        // Authorization is a singleton field: the first line wins.
        let authorization: Option<CString> = r
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| cgi_cstring("Authorization", v));

        let env: HashMap<Box<[u8]>, CString> = r
            .server_vars
            .iter()
            .filter_map(|(k, v)| match CString::new(v.as_bytes()) {
                Ok(v) => Some((k.as_bytes().into(), v)),
                Err(_) => {
                    tracing::warn!(target: "rapira", "server var {k} carries a NUL byte; dropped");
                    None
                }
            })
            .collect();

        let (remote_addr, remote_port) = cgi_addr_strings(&r.remote);

        Self {
            method: cgi_cstring("REQUEST_METHOD", r.method.as_bytes()),
            query: cgi_cstring("QUERY_STRING", r.query.as_bytes()),
            uri: cgi_cstring("REQUEST_URI", r.uri.as_bytes()),
            script: cgi_cstring(
                "SCRIPT_FILENAME",
                r.script_filename.to_string_lossy().as_bytes(),
            ),
            cookie,
            authorization,
            ctype: r
                .content_type
                .as_deref()
                .map(|s| cgi_cstring("CONTENT_TYPE", s)),
            env,
            folded_headers,
            remote_addr,
            remote_port,
            server_port: r.server_port.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// Nothing recorded yet.
    NotSent,
    /// A head has been recorded; no body yet.
    HeadSent,
    /// Body output began *during the handler* (not the teardown flush).
    BodyStreamed,
}

pub struct Context {
    pub req: Request,
    /// CGI materialization; None on the dispatcher path, which binds no server
    /// context and never runs the CGI callbacks.
    pub c: Option<ReqC>,
    pub sender: Option<Sender<Frame>>,
    pub head: Option<ResponseHead>,
    pub body: Vec<u8>,
    pub stream: StreamState,
    pub tearing_down: bool,
}

impl Context {
    pub fn new(req: Request, sender: Sender<Frame>, superglobals: bool) -> Self {
        let c = superglobals.then(|| ReqC::build(&req));
        Self {
            req,
            c,
            sender: Some(sender),
            head: None,
            body: Vec::new(),
            stream: StreamState::NotSent,
            tearing_down: false,
        }
    }

    pub fn is_truncated(&self, errored: bool) -> bool {
        errored && self.stream == StreamState::BodyStreamed
    }

    pub fn commit_head(&mut self, mut status: u16, mut headers: FieldLines) {
        headers.retain(|(name, value)| {
            if !name.eq_ignore_ascii_case("status") {
                return true;
            }
            match status_field_code(value) {
                Some(code) => status = code,
                // Dropped either way, so without this the app sees its 404 silently served
                // as a 200 with nothing logged anywhere.
                None => tracing::warn!(
                    target: "php",
                    "ignored malformed Status field {:?}; status stays {status}",
                    String::from_utf8_lossy(value)
                ),
            }
            false
        });
        self.head = Some(ResponseHead { status, headers });
        self.stream = StreamState::HeadSent;
    }

    /// Seal the buffered response as a Head+Chunk+End event trio (Head skipped
    /// when none was recorded, Chunk when the body is empty). `take()` on the
    /// sender keeps a second call a no-op.
    pub fn finish(&mut self, truncated: bool) {
        let Some(tx) = self.sender.take() else {
            return;
        };
        let body = std::mem::take(&mut self.body);
        if let Some(head) = self.head.take() {
            let bodiless = matches!(head.status, 204 | 304)
                || (100..200).contains(&head.status)
                || self.req.method.eq_ignore_ascii_case("HEAD");
            let body_coded = head
                .headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("content-encoding"));
            // no length for a truncated body: the absent Content-Length (chunked
            // on the wire) is what lets the client detect the cut
            let content_length = (!bodiless && !truncated).then_some(body.len() as u64);
            let _ = tx.blocking_send(Frame::Head {
                head,
                content_length,
                bodiless,
                body_coded,
            });
            // bodiless bytes still travel; the wire-side drop is the front's
            if !body.is_empty() {
                let _ = tx.blocking_send(Frame::Chunk(body.into()));
            }
        }
        let _ = tx.blocking_send(Frame::End {
            trailers: Vec::new(),
            truncated,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_field_code_takes_the_leading_integer() {
        assert_eq!(status_field_code(b"404"), Some(404));
        assert_eq!(status_field_code(b"404 Not Found"), Some(404));
        assert_eq!(status_field_code(b"  503   "), Some(503));
    }

    /// An unparseable or out-of-range value leaves the status alone. The caller drops the
    /// field either way, so a bogus one is discarded rather than shown to the client.
    #[test]
    fn status_field_code_rejects_implausible_values() {
        assert_eq!(status_field_code(b""), None);
        assert_eq!(status_field_code(b"Not Found"), None);
        assert_eq!(status_field_code(b"99"), None);
        assert_eq!(status_field_code(b"600"), None);
        assert_eq!(status_field_code(b"70000"), None);
    }

    fn head_of(status: u16, headers: &[(&str, &str)]) -> ResponseHead {
        let mut ctx = Context {
            req: Request {
                method: String::new(),
                uri: String::new(),
                target: None,
                authority: None,
                https: false,
                query: String::new(),
                protocol: String::new(),
                remote: Addr::Inet(([127, 0, 0, 1], 8080).into()),
                server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
                server_name: String::new(),
                server_port: 8080,
                script_name: String::new(),
                document_root: String::new(),
                script_filename: PathBuf::new(),
                headers: Vec::new(),
                server_vars: Vec::new(),
                content_type: None,
                content_length: -1,
                body: Body::Raw(Box::new(std::io::empty())),
                received_at: None,
                tls: None,
            },
            c: None,
            sender: None,
            head: None,
            body: Vec::new(),
            stream: StreamState::NotSent,
            tearing_down: false,
        };
        let headers = headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.as_bytes().to_vec()))
            .collect();
        ctx.commit_head(status, headers);
        ctx.head.expect("commit_head records a head")
    }

    /// The whole `Status:` feature lives in commit_head's retain closure: the field sets the
    /// code, never reaches the client, and the match is case-insensitive because php-src
    /// passes whatever spelling the script wrote.
    #[test]
    fn commit_head_consumes_the_status_field() {
        let head = head_of(200, &[("status", "404 Not Found"), ("X-Keep", "kept")]);
        assert_eq!(head.status, 404);
        assert_eq!(head.headers.len(), 1);
        assert_eq!(head.headers[0].0, "X-Keep");
    }

    /// A `Status:` the SAPI cannot parse must still be consumed - forwarding it would put a
    /// literal `Status:` field on the wire - but it must not invent a code.
    #[test]
    fn commit_head_drops_an_unparseable_status_without_changing_the_code() {
        let head = head_of(201, &[("Status", "NotFound")]);
        assert_eq!(head.status, 201);
        assert!(head.headers.is_empty());
    }

    /// RFC 3875 §6.3.3 makes `Status` the script's own result code, and §6.2.1 puts the
    /// conversion on the server - so it is authoritative over whatever code php-src had
    /// already recorded from `http_response_code()`.
    #[test]
    fn commit_head_lets_the_status_field_override_the_recorded_code() {
        assert_eq!(head_of(500, &[("Status", "404")]).status, 404);
    }
}
