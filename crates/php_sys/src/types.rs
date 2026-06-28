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
        let cookie = r
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();

        let authorization: String = r
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();

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
            cookie: CString::new(cookie.as_bytes()).unwrap_or_default(),
            authorization: CString::new(authorization.as_bytes()).unwrap_or_default(),
            ctype: r
                .content_type
                .as_deref()
                .map(|s| CString::new(s.as_bytes()).unwrap_or_default()),
            env,
        }
    }
}

pub struct Context {
    pub req: Request,
    pub c: ReqC,
    pub sender: Option<Sender<Frame>>,
    pub headers_sent: bool,
}

impl Context {
    pub fn new(req: Request, sender: Sender<Frame>) -> Self {
        let c = ReqC::build(&req);
        Self {
            req,
            c,
            sender: Some(sender),
            headers_sent: false,
        }
    }

    pub fn finish(&mut self) {
        self.sender = None;
    }
}
