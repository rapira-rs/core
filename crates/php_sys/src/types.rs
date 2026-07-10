use bytes::Bytes;
use std::collections::HashMap;
use std::ffi::CString;
use std::io::Read;
use std::path::PathBuf;
use tokio::sync::mpsc::Sender;

#[derive(Debug, Clone)]
pub enum Mode {
    Classic,
    Worker(PathBuf),
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok = 0,
    Bailout = 1,
    Exit = 2,
    Throw = 3,
}

pub enum Frame {
    Head(ResponseHead),
    Body(Bytes),
    /// Terminal marker: the worker finished this response intentionally. `truncated`
    /// means PHP errored *after it had already begun streaming its body* to the
    /// consumer, so the body may be incomplete. A response whose head/body are
    /// flushed atomically at teardown (buffered output) or synthesized as a
    /// head-only error is complete, not truncated. A channel that closes without
    /// `End` means the worker died (panic / dropped job / pool shutdown).
    End {
        truncated: bool,
    },
}

pub struct Job {
    pub ctx: Context,
}

pub struct Request {
    pub method: String,
    pub uri: String,
    pub https: bool,
    pub query: String,
    pub protocol: String,
    pub remote_addr: String,
    pub server_name: String,
    pub server_port: String,
    pub remote_port: String,
    pub script_name: String,
    pub document_root: String,
    pub script_filename: PathBuf,
    pub headers: Vec<(String, Vec<u8>)>, // values as bytes: latin1/binary-safe
    pub server_vars: Vec<(String, String)>,
    pub content_type: Option<String>,
    pub content_length: i64, // -1 if unknown
    pub body: Box<dyn Read + Send>,
}

pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>,
}

pub struct ReqC {
    pub method: CString,
    pub query: CString,
    pub uri: CString,
    pub script: CString,
    pub ctype: Option<CString>,
    pub cookie: CString,
    pub authorization: CString,
    pub env: HashMap<Box<[u8]>, CString>,
}

impl ReqC {
    pub fn build(r: &Request) -> Self {
        let mut cookie: Vec<u8> = Vec::new();
        for (_, v) in r
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("cookie"))
        {
            if !cookie.is_empty() {
                cookie.extend_from_slice(b"; ");
            }
            cookie.extend_from_slice(v);
        }

        // Build the CString straight from the header bytes — no owned-String detour.
        let authorization: CString = r
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map_or_else(CString::default, |(_, v)| {
                CString::new(v.as_slice()).unwrap_or_default()
            });

        let env: HashMap<Box<[u8]>, CString> = r
            .server_vars
            .iter()
            .filter_map(|(k, v)| Some((k.as_bytes().into(), CString::new(v.as_bytes()).ok()?)))
            .collect();

        Self {
            method: CString::new(r.method.as_bytes()).unwrap_or_default(),
            query: CString::new((r.query).as_bytes()).unwrap_or_default(),
            uri: CString::new(r.uri.as_bytes()).unwrap_or_default(),
            script: CString::new(r.script_filename.to_string_lossy().to_string())
                .unwrap_or_default(),
            cookie: CString::new(cookie).unwrap_or_default(),
            authorization,
            ctype: r
                .content_type
                .as_deref()
                .map(|s| CString::new(s.as_bytes()).unwrap_or_default()),
            env,
        }
    }
}

/// How far the response stream has progressed to the consumer. Monotonic
/// (`NotSent` → `HeadSent` → `BodyStreamed`), which makes the illegal
/// "body before head" state unrepresentable and is the single source of truth
/// for the old `headers_sent`/`body_started` checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// No frame emitted yet.
    NotSent,
    /// A `Frame::Head` has been emitted; no body yet.
    HeadSent,
    /// At least one `Frame::Body` has been streamed to the consumer.
    BodyStreamed,
}

pub struct Context {
    pub req: Request,
    pub c: ReqC,
    pub sender: Option<Sender<Frame>>,
    /// Head/body progression to the consumer.
    pub stream: StreamState,
    /// True once the handler has returned and we are flushing at teardown, so a
    /// buffered body pushed out by the teardown flush does not advance `stream` to
    /// `BodyStreamed` — only body streamed *during* the handler counts as truncation.
    pub tearing_down: bool,
}

impl Context {
    pub fn new(req: Request, sender: Sender<Frame>) -> Self {
        let c = ReqC::build(&req);
        Self {
            req,
            c,
            sender: Some(sender),
            stream: StreamState::NotSent,
            tearing_down: false,
        }
    }

    /// The response body is truncated iff the request `errored` *after* it had begun
    /// streaming its body to the consumer. A buffered or head-only response — whose
    /// head/body are flushed atomically at teardown — is complete, not truncated.
    /// Order-independent: [`Self::tearing_down`] keeps `stream` from advancing to
    /// `BodyStreamed` during the teardown flush, so this can be read at any point.
    pub fn is_truncated(&self, errored: bool) -> bool {
        errored && self.stream == StreamState::BodyStreamed
    }

    /// Seal the response stream: emit the terminal [`Frame::End`], then drop the
    /// sender. Pass the truncation flag from [`Self::is_truncated`] (see [`Frame::End`]).
    pub fn finish(&mut self, truncated: bool) {
        if let Some(tx) = self.sender.take() {
            let _ = tx.blocking_send(Frame::End { truncated });
        }
    }
}
