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
        received_at: 0.0,
    }
}

/// Drain a response to `(status, body)`. Status is 0 if no head was produced
/// (or the worker died before sealing a frame).
pub fn drain(mut rx: mpsc::Receiver<Frame>) -> (u16, String) {
    match rx.blocking_recv() {
        Some(f) => (
            f.head.map_or(0, |h| h.status),
            String::from_utf8_lossy(&f.body).into_owned(),
        ),
        None => (0, String::new()),
    }
}

/// Async sibling of `php_lock` for `#[tokio::test]`.
pub async fn php_lock_async() -> tokio::sync::MutexGuard<'static, ()> {
    init_php_env();
    PHP_LOCK_ASYNC.lock().await
}

/// Async sibling of `drain`: drain a response to `(status, body)` inside a runtime.
pub async fn drain_async(mut rx: mpsc::Receiver<Frame>) -> (u16, String) {
    match rx.recv().await {
        Some(f) => (
            f.head.map_or(0, |h| h.status),
            String::from_utf8_lossy(&f.body).into_owned(),
        ),
        None => (0, String::new()),
    }
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
    let h = r.handle().expect("handle");
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

/// Install the capturing subscriber once (records all `tracing` output — plus
/// anything still on the `log` facade — into `LOG_CAPTURE`).
pub fn init_log_capture() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        // No filter: every record is captured, down to trace-level masked diagnostics.
        let _ = tracing_subscriber::registry().with(CaptureLayer).try_init();
    });
}
