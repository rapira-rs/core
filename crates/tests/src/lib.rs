use php_sys::{Frame, Mode, Rapira, Request};
use std::env::set_var;
use std::path::{Path, PathBuf};
use std::sync::{self, Mutex, Once, PoisonError};
use tokio::sync::mpsc;

static PHP_LOCK: Mutex<()> = Mutex::new(());
static PHP_ENV: Once = Once::new();
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

pub fn set_phprc(_php: &sync::MutexGuard<'static, ()>, ini: &Path) {
    // SAFETY: PHP_LOCK is held for as long as `_php` lives, so nothing reads the environment
    // concurrently.
    unsafe { set_var("PHPRC", ini) };
}

/// `php_lock()` plus a PHPRC override for this whole test binary.
pub fn php_lock_with_ini(ini: &Path) -> sync::MutexGuard<'static, ()> {
    let guard = php_lock();
    set_phprc(&guard, ini);
    guard
}

/// Build a minimal `GET` request for `uri`, with `$_SERVER` metadata pointing at `fixture_name`.
pub fn req(uri: &str, fixture_name: &str) -> Request {
    let query = uri.split_once('?').map(|x: (&str, &str)| x.1);
    Request {
        document_root: String::new(),
        https: false,
        method: "GET".into(),
        uri: uri.into(),
        target: None,
        authority: None,
        query: query.unwrap_or("").into(),
        protocol: "HTTP/1.1".into(),
        remote: php_sys::types::Addr::Inet(([127, 0, 0, 1], 8080).into()),
        server: php_sys::types::Addr::Inet(([127, 0, 0, 1], 8080).into()),
        server_name: "localhost".into(),
        server_port: 8080,
        script_filename: fixture(fixture_name),
        script_name: "/index.php".into(),
        headers: vec![],
        server_vars: vec![],
        content_type: None,
        content_length: 0,
        body: php_sys::types::Body::Raw(Box::new(std::io::empty())),
        received_at: None,
        tls: None,
    }
}

/// A response stream collected to its `End` (or to the producer dying).
#[derive(Default)]
pub struct Resp {
    pub interim: Vec<php_sys::ResponseHead>,
    pub head: Option<php_sys::ResponseHead>,
    pub content_length: Option<u64>,
    pub bodiless: bool,
    pub body: Vec<u8>,
    pub trailers: Vec<(String, Vec<u8>)>,
    pub truncated: bool,
    /// An `End` frame arrived; false = the producer died first.
    pub ended: bool,
    /// Head frames seen; `head` keeps only the last, so a duplicate would
    /// otherwise be invisible.
    pub heads: u32,
}

impl Resp {
    /// 0 = no head (producer died, or the response never recorded one).
    pub fn status(&self) -> u16 {
        self.head.as_ref().map_or(0, |h| h.status)
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.head
            .as_ref()?
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
    }

    pub fn body_string(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Fold one frame in; true when the stream is over.
    fn fold(&mut self, frame: Frame) -> bool {
        match frame {
            Frame::Interim(h) => self.interim.push(h),
            Frame::Head {
                head,
                content_length,
                bodiless,
                ..
            } => {
                self.heads += 1;
                self.head = Some(head);
                self.content_length = content_length;
                self.bodiless = bodiless;
            }
            Frame::Chunk(b) => self.body.extend_from_slice(&b),
            Frame::File { file, offset, len } => match read_slice(&file, offset, len) {
                Ok(bytes) => self.body.extend_from_slice(&bytes),
                Err(e) => panic!("reading a File frame: {e}"),
            },
            Frame::End {
                trailers,
                truncated,
            } => {
                self.trailers = trailers;
                self.truncated = truncated;
                self.ended = true;
                return true;
            }
        }
        false
    }
}

fn read_slice(file: &std::fs::File, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut out = vec![0u8; usize::try_from(len).unwrap_or(usize::MAX)];
    let mut done = 0usize;
    while done < out.len() {
        let n = file.read_at(&mut out[done..], offset + done as u64)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    out.truncate(done);
    Ok(out)
}

/// Poll until a first frame arrives (or `deadline` passes), then collect the
/// stream. None = nothing arrived in time. A producer that died with no
/// frames yields `Resp::default()` (no head, not ended).
pub fn drain_resp_deadline(
    rx: &mut mpsc::Receiver<Frame>,
    deadline: std::time::Instant,
) -> Option<Resp> {
    loop {
        match rx.try_recv() {
            Ok(frame) => {
                let mut resp = Resp::default();
                if !resp.fold(frame) {
                    while let Some(f) = rx.blocking_recv() {
                        if resp.fold(f) {
                            break;
                        }
                    }
                }
                return Some(resp);
            }
            Err(mpsc::error::TryRecvError::Disconnected) => return Some(Resp::default()),
            Err(mpsc::error::TryRecvError::Empty) => {
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

/// Collect a whole response stream.
pub fn drain_resp(mut rx: mpsc::Receiver<Frame>) -> Resp {
    let mut resp = Resp::default();
    while let Some(frame) = rx.blocking_recv() {
        if resp.fold(frame) {
            break;
        }
    }
    resp
}

/// Drain a response to `(status, body)`. Status is 0 if no head was produced
/// (or the worker died before sealing one).
pub fn drain(rx: mpsc::Receiver<Frame>) -> (u16, String) {
    let r = drain_resp(rx);
    (r.status(), r.body_string())
}

/// Async sibling of `php_lock` for `#[tokio::test]`.
pub async fn php_lock_async() -> tokio::sync::MutexGuard<'static, ()> {
    init_php_env();
    PHP_LOCK_ASYNC.lock().await
}

/// Async sibling of `drain_resp`.
pub async fn drain_resp_async(mut rx: mpsc::Receiver<Frame>) -> Resp {
    let mut resp = Resp::default();
    while let Some(frame) = rx.recv().await {
        if resp.fold(frame) {
            break;
        }
    }
    resp
}

/// Async sibling of `drain`.
pub async fn drain_async(rx: mpsc::Receiver<Frame>) -> (u16, String) {
    let r = drain_resp_async(rx).await;
    (r.status(), r.body_string())
}

fn init_php_env() {
    PHP_ENV.call_once(|| {
        // SAFETY: the Once runs this exactly once, before any Rapira::start /
        // php_module_startup; mirrors the existing `unsafe { set_var }` usage in the
        // test bodies (e.g. getenv_classic, basic_tests.rs).
        unsafe {
            set_var(
                // https://www.php.net/manual/en/configuration.file.php
                "PHPRC",
                concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/ini/shared/php.ini"),
            );
        }
    });
}

/// One captured record. PHP diagnostics are asserted on level and target, not just text.
#[derive(Debug)]
pub struct Captured {
    pub level: tracing::Level,
    pub target: String,
    pub message: String,
    /// The `context` field, empty when the record carries none. Rapira\log()
    /// puts its JSON-encoded context array here.
    pub context: String,
}

static LOG_CAPTURE: Mutex<Vec<Captured>> = Mutex::new(Vec::new());

/// The captured records. A failing assertion holds this guard while it panics, so the lock is
/// recovered rather than unwrapped: otherwise one real failure poisons the buffer and every
/// later test in the binary dies on `PoisonError` instead of its own assertion.
pub fn captured() -> sync::MutexGuard<'static, Vec<Captured>> {
    LOG_CAPTURE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Collects the `message` and `context` fields; any `log.*` metadata fields from
/// still-bridged records are ignored.
#[derive(Default)]
struct Msg {
    message: String,
    context: String,
}

impl Msg {
    fn slot(&mut self, name: &str) -> Option<&mut String> {
        match name {
            "message" => Some(&mut self.message),
            "context" => Some(&mut self.context),
            _ => None,
        }
    }
}

impl tracing::field::Visit for Msg {
    fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
        if let Some(slot) = self.slot(f.name()) {
            *slot = v.to_owned();
        }
    }
    // A `%value` field arrives here: tracing records Display through record_debug,
    // wrapped in a format_args! whose Debug forwards to Display.
    fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
        if let Some(slot) = self.slot(f.name()) {
            *slot = format!("{v:?}");
        }
    }
}

struct CaptureLayer;

impl<S: tracing::Subscriber> tracing_subscriber::layer::Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let norm = tracing_log::NormalizeEvent::normalized_metadata(event);
        let meta = norm.as_ref().unwrap_or_else(|| event.metadata());
        let mut msg = Msg::default();
        event.record(&mut msg);
        captured().push(Captured {
            level: *meta.level(),
            target: meta.target().to_owned(),
            message: msg.message,
            context: msg.context,
        });
    }
}

/// One `app`-target record left by `\Rapira\log()`: level, message, context JSON.
pub type AppRecord = (tracing::Level, String, String);

/// Run `script` in classic mode and return the `app`-target records it left, in
/// emission order. The fixture must echo `logged` as its last act, so a script
/// that died half way cannot masquerade as one that logged nothing.
pub fn app_records(script: &str) -> Vec<AppRecord> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear(); // drop anything captured by earlier tests

    let r = Rapira::start(Mode::Classic).expect("classic boot");
    let h = r.handle();
    let (status, body) = drain(h.handle_blocking(req("/", script)).expect("dispatch"));
    drop(h);
    r.shutdown();

    assert_eq!(status, 200, "{script} must run clean (body: {body:?})");
    assert!(body.contains("logged"), "{script} ran to the end: {body:?}");

    captured()
        .iter()
        .filter(|c| c.target == "app")
        .map(|c| (c.level, c.message.clone(), c.context.clone()))
        .collect()
}

/// The one `app` record `script` was expected to leave. Asserting the count
/// rather than taking the first means a stray extra record fails the test
/// instead of going unnoticed.
pub fn app_record(script: &str) -> AppRecord {
    let records = app_records(script);
    assert_eq!(
        records.len(),
        1,
        "{script} must log exactly one app record (got {records:?})"
    );
    records.into_iter().next().expect("checked above")
}

/// Install the capturing subscriber once (records all `tracing` output - plus
/// anything still on the `log` facade - into `LOG_CAPTURE`).
pub fn init_log_capture() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        // No filter: every record is captured, down to trace-level masked diagnostics.
        let _ = tracing_subscriber::registry().with(CaptureLayer).try_init();
    });
}
