//! Registers native rapira extensions and drives each on a shared runtime. The
//! master runs `prepare_all` before forking; each forked worker then drives its
//! extensions with `serve_worker` until they finish or a drain signal arrives.

// Signal handling below calls POSIX APIs unconditionally; fail fast with a clear
// message instead of scattered libc symbol errors.
#[cfg(not(unix))]
compile_error!("rapira supports Unix (Linux/macOS) only");

use extension_api::{Extension, Php, PrepareCtx};
use php_sys::RapiraHandle;
use std::future::Future;
use std::io::Cursor;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::watch;
use tokio::task::JoinSet;

type Outcome = std::result::Result<(), String>;
type BoxFuture = Pin<Box<dyn Future<Output = Outcome> + Send>>;

/// Object-safe shim so the host can touch the SAME extension value twice:
/// `prepare` in the master (pre-fork), `launch` in the worker (post-fork). The
/// extension value — including any `PreparedListener` it stored — is what
/// crosses the fork.
trait ErasedExt: Send {
    fn prepare(&mut self, ctx: &mut PrepareCtx) -> anyhow::Result<()>;
    fn launch(self: Box<Self>, php: Php, stop: watch::Receiver<bool>, grace: Duration)
    -> BoxFuture;
}

impl<E: Extension> ErasedExt for E {
    fn prepare(&mut self, ctx: &mut PrepareCtx) -> anyhow::Result<()> {
        Extension::prepare(self, ctx)
    }

    fn launch(
        self: Box<Self>,
        php: Php,
        stop: watch::Receiver<bool>,
        grace: Duration,
    ) -> BoxFuture {
        Box::pin(drive(*self, php, stop, grace))
    }
}

struct Registered {
    name: String,
    ext: Box<dyn ErasedExt>,
}

/// Collects native extensions, then drives them all with one `run` call.
#[derive(Default)]
pub struct ExtensionRuntime {
    exts: Vec<Registered>,
}

impl ExtensionRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct `E` (via `init`, injecting its config) and stage it. A duplicate name
    /// is a hard error.
    pub fn register<E: Extension>(&mut self, config: E::Config) -> anyhow::Result<()> {
        let ext = E::init(config);
        let name = ext.name().to_string();
        if self.exts.iter().any(|e| e.name == name) {
            anyhow::bail!("duplicate extension {name:?}");
        }
        self.exts.push(Registered {
            name,
            ext: Box::new(ext),
        });
        Ok(())
    }

    /// Master-side, pre-fork: run every extension's `prepare` in registration
    /// order; the first error aborts boot, tagged with the extension name.
    pub fn prepare_all(&mut self, ctx: &mut PrepareCtx) -> anyhow::Result<()> {
        use anyhow::Context;
        for Registered { name, ext } in &mut self.exts {
            ext.prepare(ctx)
                .with_context(|| format!("extension {name}: prepare failed"))?;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.exts.is_empty()
    }

    /// Spawn every extension on a shared runtime; one `Php` (the single entry script) is
    /// cloned to each. The returned guard drives them to completion / shutdown.
    pub fn run(self, rapira: RapiraHandle, script: PathBuf) -> Running {
        self.run_with_grace(rapira, script, Duration::from_secs(30))
    }

    /// As [`run`](Self::run), with a custom per-extension graceful-shutdown budget.
    pub fn run_with_grace(self, rapira: RapiraHandle, script: PathBuf, grace: Duration) -> Running {
        let php = Php::new(Arc::new(RapiraBackend::new(rapira, script.clone())), script);
        let (stop_tx, stop_rx) = watch::channel(false);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_time() // the shutdown timeout in `drive`; extensions own their own IO
            .thread_name("rapira-ext")
            .build()
            .expect("build extension runtime");

        let mut tasks: JoinSet<Result<(), String>> = JoinSet::new();
        for Registered { name, ext } in self.exts {
            let (php, stop) = (php.clone(), stop_rx.clone());
            let fut = ext.launch(php, stop, grace);
            tasks.spawn_on(
                async move {
                    let outcome = fut.await;
                    match &outcome {
                        Ok(()) => log::info!(target: "ext", "{name} finished"),
                        Err(msg) => log::error!(target: "ext", "{name}: {msg}"),
                    }
                    outcome
                },
                rt.handle(),
            );
        }

        Running { rt, tasks, stop_tx }
    }
}

/// The production [`extension_api::Backend`]: bridges `Php::exec` onto the PHP worker
/// pool. Owns the entry script's CGI vars, computed once at construction instead of
/// per request.
struct RapiraBackend {
    rapira: RapiraHandle,
    /// SCRIPT_FILENAME
    filename: PathBuf,
    /// DOCUMENT_ROOT (the script's parent directory)
    document_root: String,
    /// SCRIPT_NAME, e.g. "/index.php"
    script_name: String,
}

impl RapiraBackend {
    fn new(rapira: RapiraHandle, filename: PathBuf) -> Self {
        let document_root = filename
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let script_name = filename
            .file_name()
            .map_or_else(|| "/".to_string(), |f| format!("/{}", f.to_string_lossy()));
        Self {
            rapira,
            filename,
            document_root,
            script_name,
        }
    }

    /// The one place the `extension_api::Request → php_sys::Request` mapping lives.
    fn to_request(&self, mut req: extension_api::Request) -> php_sys::Request {
        req.headers = combine_field_lines(std::mem::take(&mut req.headers));
        let query = req.uri.split_once('?').map_or("", |(_, q)| q).to_string();
        // Carried as raw bytes, like every other header value: php-src takes the
        // multipart boundary out of this verbatim, so a lossy decode would turn a
        // non-UTF-8 boundary into U+FFFD and the body's real boundary would never match.
        let content_type = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone());

        php_sys::Request {
            method: req.method,
            https: req.https,
            query,
            protocol: req.protocol,
            remote_addr: req.remote_addr,
            server_name: req.server_name,
            server_port: req.server_port.to_string(),
            remote_port: req.remote_port.to_string(),
            script_name: self.script_name.clone(),
            document_root: self.document_root.clone(),
            script_filename: self.filename.clone(),
            content_type,
            content_length: req.body.len() as i64,
            body: Box::new(Cursor::new(req.body)),
            headers: req.headers,
            server_vars: Vec::new(),
            uri: req.uri,
        }
    }
}

/// Make `php_sys::Request::headers`' "at most one entry per field name" invariant true rather
/// than merely documented. The front an extension is built on normally groups by name already
/// (this is a no-op for `rapira_pingora`), but the invariant governs the CGI `$_SERVER` mapping
/// and every consumer of a violation disagrees about which duplicate wins: `HTTP_*` keeps the
/// last, the cookie fold keeps all, and the `AUTH_TYPE` lookup keeps the first. Normalising at
/// the one funnel every extension passes through costs a pass over ~20 short names.
///
/// Case-insensitive, because `cgi_header_name` uppercases: `Cookie` and `cookie` are distinct
/// `String`s that land on one CGI variable.
fn combine_field_lines(lines: Vec<(String, Vec<u8>)>) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::with_capacity(lines.len());
    for (name, value) in lines {
        let Some(i) = out.iter().position(|(n, _)| n.eq_ignore_ascii_case(&name)) else {
            out.push((name, value));
            continue;
        };
        let Some(separator) = extension_api::field_line_separator(&name) else {
            log::warn!("dropped a repeated {name} field line: not a list field");
            continue;
        };
        log::warn!("combined a repeated {name} field line; extensions should submit one entry");
        let combined: &mut Vec<u8> = &mut out[i].1;
        combined.extend_from_slice(separator);
        combined.extend_from_slice(&value);
    }
    out
}

impl extension_api::Backend for RapiraBackend {
    /// Submit `req` and collect the whole response — the worker seals it into a
    /// single frame, so the caller wakes once per response (the error contract lives
    /// on `Php::exec`).
    fn exec(
        &self,
        req: extension_api::Request,
    ) -> Pin<Box<dyn Future<Output = extension_api::Result<extension_api::Response>> + Send + '_>>
    {
        Box::pin(async move {
            let mut rx = self.rapira.handle(self.to_request(req)).await?;
            let Some(frame) = rx.recv().await else {
                return Err(anyhow::anyhow!(
                    "php worker died mid-response (channel closed without a response)"
                ));
            };
            if frame.truncated {
                return Err(anyhow::anyhow!("php crashed mid-response; body truncated"));
            }
            let Some(head) = frame.head else {
                return Err(anyhow::anyhow!("php produced no response head"));
            };
            // Header/body bytes pass through unchanged: PHP may emit latin1/binary.
            Ok(extension_api::Response {
                status: head.status,
                headers: head.headers,
                body: frame.body.into(),
            })
        })
    }
}

/// Drive one extension: run until it finishes or the host asks it to stop. On stop the
/// `run` future is dropped (releasing `&mut ext`), then `shutdown` drains it (bounded by `grace`).
async fn drive<E: Extension>(
    mut ext: E,
    php: Php,
    mut stop: watch::Receiver<bool>,
    grace: Duration,
) -> Outcome {
    let finished = {
        let run = ext.run(php);
        tokio::pin!(run);
        tokio::select! {
            outcome = &mut run => Some(outcome),
            // Also resolves if the sender is dropped.
            _ = stop.wait_for(|stopping| *stopping) => None,
        }
    };
    match finished {
        Some(outcome) => outcome.map_err(|e| format!("run failed: {e:#}")),
        None => match tokio::time::timeout(grace, ext.shutdown()).await {
            Ok(result) => result.map_err(|e| format!("shutdown failed: {e:#}")),
            Err(_) => Err("shutdown timed out".into()),
        },
    }
}

/// Build a sigset from a signal list.
fn sigset(signals: &[libc::c_int]) -> libc::sigset_t {
    // SAFETY: operates on a stack-owned, freshly-initialized signal set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for &sig in signals {
            libc::sigaddset(&mut set, sig);
        }
        set
    }
}

/// Block until one of `signals` (already blocked) is delivered; return its
/// number. No timeout: the caller waits on the signal like a channel receive.
/// `sigwait` is portable across macOS, Linux, and BSD, unlike `sigtimedwait`,
/// which Darwin lacks.
///
/// https://man7.org/linux/man-pages/man2/sigwaitinfo.2.html
fn wait_signal(signals: &[libc::c_int]) -> libc::c_int {
    // SAFETY: `set` and `sig` are stack values live for the whole call.
    unsafe {
        let set = sigset(signals);
        let mut sig: libc::c_int = 0;
        libc::sigwait(&set, &mut sig);
        sig
    }
}

/// A cheap handle that asks a [`Running`] host to stop gracefully. Callable
/// from plain threads (`watch::Sender::send` needs no runtime); used by the
/// max_requests recycle hook and tests.
#[derive(Clone)]
pub struct Stopper(watch::Sender<bool>);

impl Stopper {
    pub fn stop(&self) {
        let _ = self.0.send(true);
    }
}

/// Drives the extension tasks. `serve_worker` stops them on a worker signal;
/// `join` only waits; `drop` is the safety net.
pub struct Running {
    rt: Runtime,
    tasks: JoinSet<Outcome>,
    stop_tx: watch::Sender<bool>,
}

impl Running {
    /// Wait for every extension to finish on its own (no signal handling). For tests
    /// and run-to-completion extensions.
    pub fn join(mut self) -> Vec<Outcome> {
        self.drain_all()
    }

    /// Ask every extension to stop, then drain and return their outcomes — the on-demand
    /// graceful-stop path (no signal), for callers that drive shutdown themselves.
    pub fn stop(self) -> Vec<Outcome> {
        let _ = self.stop_tx.send(true);
        self.join()
    }

    /// External graceful-stop trigger (max_requests recycle, tests).
    pub fn stopper(&self) -> Stopper {
        Stopper(self.stop_tx.clone())
    }

    /// Forked-worker entry: run until done OR a QUIT/INT arrives — first signal
    /// drains, a second one force-exits 131. The master's fork bracket owns
    /// child signal hygiene: dispositions reset to SIG_DFL, USR1/USR2 ignored,
    /// mask exactly {QUIT, INT} for the watcher here, TERM left at SIG_DFL so
    /// the master's escalation kills fast.
    pub fn serve_worker(mut self) -> Vec<Outcome> {
        let stop_tx = self.stop_tx.clone();
        std::thread::Builder::new()
            .name("rapira-worker-signal".into())
            .spawn(move || {
                let sig = wait_signal(&[libc::SIGQUIT, libc::SIGINT]);
                log::info!(target: "rapira", "signal {sig} received; draining worker");
                let _ = stop_tx.send(true);
                let _ = wait_signal(&[libc::SIGQUIT, libc::SIGINT]);
                log::warn!(target: "rapira", "second signal; forcing worker exit");
                std::process::exit(131);
            })
            .expect("spawn worker signal thread");
        self.drain_all()
    }

    /// Take the staged tasks and drive them to completion on the runtime.
    fn drain_all(&mut self) -> Vec<Outcome> {
        let mut tasks = std::mem::take(&mut self.tasks);
        self.rt.block_on(drain(&mut tasks))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Safety net for a guard dropped without serve/join/stop: ask extensions to stop,
        // then drain (each `shutdown` bounded by the host's grace in `drive`). After
        // serve/join/stop the tasks are already taken, so this is a cheap no-op.
        let _ = self.stop_tx.send(true);
        let _ = self.drain_all();
    }
}

/// Collect every task's outcome; a panicked task becomes an `Err`.
async fn drain(tasks: &mut JoinSet<Outcome>) -> Vec<Outcome> {
    let mut out = Vec::with_capacity(tasks.len());
    while let Some(joined) = tasks.join_next().await {
        out.push(joined.unwrap_or_else(|_| Err("driver task panicked".into())));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host is built, registered, then consumed by `run` on one thread; it need not
    /// be `Sync`, but staged launchers must be `Send` (they move into spawned tasks).
    #[test]
    fn rapira_runtime_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ExtensionRuntime>();
    }

    fn lines(pairs: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.as_bytes().to_vec()))
            .collect()
    }

    fn value<'a>(pairs: &'a [(String, Vec<u8>)], name: &str) -> Option<&'a [u8]> {
        pairs
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_slice())
    }

    /// `php_sys::Request::headers` promises at most one entry per name, and the consumers of a
    /// violation disagree about which duplicate wins — `HTTP_*` keeps the last, the cookie fold
    /// keeps all, `AUTH_TYPE` keeps the first. An extension that submits repeats must not be
    /// able to produce that split.
    #[test]
    fn repeated_field_lines_are_combined_before_php_sees_them() {
        let combined = combine_field_lines(lines(&[
            ("Cookie", "a=1"),
            ("cookie", "b=2"),
            ("X-Forwarded-For", "1.2.3.4"),
            ("X-Forwarded-For", "5.6.7.8"),
            ("Accept", "text/*"),
        ]));
        assert_eq!(combined.len(), 3, "one entry per field name");
        assert_eq!(value(&combined, "cookie"), Some(&b"a=1; b=2"[..]));
        assert_eq!(
            value(&combined, "x-forwarded-for"),
            Some(&b"1.2.3.4, 5.6.7.8"[..])
        );
        assert_eq!(value(&combined, "accept"), Some(&b"text/*"[..]));
    }

    /// Joining a singleton field corrupts it: a second `Authorization` folded into the first
    /// lands inside the credential php-src base64-decodes.
    #[test]
    fn repeated_singleton_field_lines_keep_only_the_first() {
        let combined = combine_field_lines(lines(&[
            ("Authorization", "Basic dXNlcjpwYXNz"),
            ("Authorization", "Basic ZXZpbDpldmls"),
        ]));
        assert_eq!(
            value(&combined, "authorization"),
            Some(&b"Basic dXNlcjpwYXNz"[..])
        );
    }

    /// The reaper dequeues a blocked, pending signal via `sigwait` instead of letting it
    /// run the default (terminate) action — the basis of graceful shutdown.
    #[test]
    fn sigwait_reaps_a_blocked_signal() {
        let set = sigset(&[libc::SIGTERM]);
        // SAFETY: blocks SIGTERM in this thread, so `raise` leaves it pending here
        // for `sigwait` to dequeue; it never reaches the default (terminate) action.
        unsafe {
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            libc::raise(libc::SIGTERM);
        }
        assert_eq!(wait_signal(&[libc::SIGTERM]), libc::SIGTERM);
    }
}
