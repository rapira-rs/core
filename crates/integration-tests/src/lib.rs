use log;
use php_sys::{Frame, Request};
use std::env::set_var;
use std::path::{Path, PathBuf};
use std::sync::{self, Mutex, Once, PoisonError};
use tokio::sync::mpsc;

static PHP_LOCK: Mutex<()> = Mutex::new(());
static PHP_ENV: Once = Once::new();
// async sibling of PHP_LOCK for the #[tokio::test] suite — a std guard held across
// .await trips clippy::await_holding_lock, so the async tests serialize on a tokio mutex.
static PHP_LOCK_ASYNC: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Absolute path to a PHP fixture shipped with this crate (robust to the test's cwd).
pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

pub fn php_lock() -> sync::MutexGuard<'static, ()> {
    init_php_env();
    PHP_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Build a minimal `GET` request for `uri`, with `$_SERVER` metadata pointing at `fixture_name`.
pub fn req(uri: &str, fixture_name: &str) -> Request {
    let query = uri.split_once('?').map(|x: (&str, &str)| x.1);
    Request {
        remote_port: "8080".into(),
        document_root: String::new(),
        https: false,
        method: "GET".into(),
        uri: uri.into(),
        query: query.unwrap_or("").into(),
        protocol: "HTTP/1.1".into(),
        remote_addr: "127.0.0.1".into(),
        server_name: "localhost".into(),
        server_port: "8080".into(),
        script_filename: fixture(fixture_name),
        script_name: "/index.php".into(),
        headers: vec![],
        server_vars: vec![],
        content_type: None,
        content_length: 0,
        body: Box::new(std::io::empty()),
    }
}

/// Drain a response stream to `(status, body)`. Status is 0 if no head frame arrived.
pub fn drain(mut rx: mpsc::Receiver<Frame>) -> (u16, String) {
    let (mut status, mut body) = (0u16, Vec::new());
    while let Some(frame) = rx.blocking_recv() {
        match frame {
            Frame::Head(h) => status = h.status,
            Frame::Body(b) => body.extend_from_slice(&b),
        }
    }
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Async sibling of `php_lock` for `#[tokio::test]`.
pub async fn php_lock_async() -> tokio::sync::MutexGuard<'static, ()> {
    init_php_env();
    PHP_LOCK_ASYNC.lock().await
}

/// Async sibling of `drain`: drain a response stream to `(status, body)` inside a runtime.
pub async fn drain_async(mut rx: mpsc::Receiver<Frame>) -> (u16, String) {
    let (mut status, mut body) = (0u16, Vec::new());
    while let Some(frame) = rx.recv().await {
        match frame {
            Frame::Head(h) => status = h.status,
            Frame::Body(b) => body.extend_from_slice(&b),
        }
    }
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Point the embedded PHP at the test php.ini so request-level compile/fatal errors
/// render into the response body. CI's php.ini-production defaults display_errors
/// off, which routes the error to the log and leaves the body empty (failboot_classic).
/// PHPRC replaces the ambient php.ini; the suite uses only core/standard, so nothing
/// is dropped. Runs once, before the first Rapira::start in the process.
fn init_php_env() {
    PHP_ENV.call_once(|| {
        // SAFETY: the Once runs this exactly once, before any Rapira::start /
        // php_module_startup; mirrors the existing `unsafe { set_var }` usage in the
        // test bodies (e.g. getenv_classic, basic_tests.rs).
        unsafe {
            set_var(
                // https://www.php.net/manual/en/configuration.file.php
                "PHPRC",
                concat!(env!("CARGO_MANIFEST_DIR"), "/tests/php.ini"),
            );
        }
    });
}

pub static LOG_CAPTURE: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct CaptureLogger;
impl log::Log for CaptureLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        if let Ok(mut buf) = LOG_CAPTURE.lock() {
            buf.push(record.args().to_string());
        }
    }
    fn flush(&self) {}
}

/// Install the capturing logger once (records all `log` output into `LOG_CAPTURE`).
pub fn init_log_capture() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = log::set_boxed_logger(Box::new(CaptureLogger));
        log::set_max_level(log::LevelFilter::Info);
    });
}
