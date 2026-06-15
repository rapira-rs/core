use std::ffi::CString;
use std::io::Read;
use std::path::PathBuf;
use bytes::Bytes;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum Mode {
    Classic,
    Worker(PathBuf),
}

pub enum Frame {
    Head(ResponseHead),
    Body(Bytes),
}

struct Job {
    ctx: Context,
}

pub struct Request {
    pub method: String,
    pub uri: String,
    pub query: String,
    pub protocol: String,
    pub remote_addr: String,
    pub server_name: String,
    pub server_port: String,
    pub script_filename: PathBuf,
    pub script_name: String,
    pub headers: Vec<(String, String)>,
    pub server_vars: Vec<(String, String)>,
    pub content_type: Option<String>,
    pub content_length: i64, // -1 if unknown
    pub body: Box<dyn Read + Send>,
}

pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

struct ReqC {
    method: CString,
    query: CString,
    uri: CString,
    script: CString,
    ctype: Option<CString>,
    cookie: CString,
}

impl ReqC {
    fn build(r: &Request) -> Self {
        let cookie = r
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();

        Self {
            method: CString::new(r.method.as_bytes()).unwrap_or_default(),
            query: CString::new(r.query.as_bytes()).unwrap_or_default(),
            uri: CString::new(r.uri.as_bytes()).unwrap_or_default(),
            script: CString::new(r.script_filename.to_string_lossy().to_string())
                .unwrap_or_default(),
            cookie: CString::new(cookie.as_bytes()).unwrap_or_default(),
            ctype: r
                .content_type
                .as_deref()
                .map(|s| CString::new(s.as_bytes()).unwrap_or_default()),
        }
    }
}

pub struct Context {
    req: Request,
    c: ReqC,
    pub(crate) tx: Option<mpsc::UnboundedSender<Frame>>,
    pub(crate) headers_sent: bool,
}

impl Context {
    fn new(req: Request, tx: mpsc::UnboundedSender<Frame>) -> Self {
        let c = ReqC::build(&req);
        Self {
            req,
            c,
            tx: Some(tx),
            headers_sent: false,
        }
    }

    fn finish(&mut self) {
        self.tx = None;
    }
}
