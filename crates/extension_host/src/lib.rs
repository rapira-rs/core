//! Registers native rapira extensions and drives each on a shared runtime. `serve`
//! runs until every extension finishes or a terminate/interrupt signal arrives; on a
//! signal it stops and drains them all — this is how rapira_core shuts down and exits.

use extension_api::{Extension, Php};
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
type Launcher = Box<dyn FnOnce(Php, watch::Receiver<bool>, Duration) -> BoxFuture + Send>;

struct Registered {
    name: String,
    launch: Launcher,
}

/// Collects native extensions, then drives them all with one `run` call.
#[derive(Default)]
pub struct ExtensionHost {
    exts: Vec<Registered>,
}

impl ExtensionHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct `E` (via `init`, injecting its config) and stage it. A duplicate name
    /// is a hard error; identity is captured for logging before `E` is moved into the
    /// launcher.
    pub fn register<E: Extension>(&mut self, config: E::Config) -> anyhow::Result<()> {
        let ext = E::init(config);
        let name = ext.name().to_string();
        if self.exts.iter().any(|e| e.name == name) {
            anyhow::bail!("duplicate extension {name:?}");
        }
        let launch: Launcher =
            Box::new(move |php, stop, grace| Box::pin(drive(ext, php, stop, grace)));
        self.exts.push(Registered { name, launch });
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
        for Registered { name, launch } in self.exts {
            let (php, stop) = (php.clone(), stop_rx.clone());
            tasks.spawn_on(
                async move {
                    let outcome = launch(php, stop, grace).await;
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
    fn to_request(&self, req: extension_api::Request) -> php_sys::Request {
        let query = req.uri.split_once('?').map_or("", |(_, q)| q).to_string();
        let content_type = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| String::from_utf8_lossy(v).into_owned());

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

/// Own process shutdown. Blocks SIGINT/SIGTERM in the calling thread so every thread
/// spawned afterwards (the PHP workers, the extension runtime) inherits the block; rapira
/// then reaps the signal with `sigwait` on a dedicated thread rather than a `sigaction`
/// handler, so it never replaces a disposition Zend re-installs per request. Call once, in
/// `main`, before booting PHP.
///
/// https://man7.org/linux/man-pages/man3/sigwait.3.html
/// https://man7.org/linux/man-pages/man2/sigaction.2.html
pub fn arm_shutdown_signals() {
    // SAFETY: operates on a stack-owned, freshly-initialized signal set.
    // https://man7.org/linux/man-pages/man3/pthread_sigmask.3.html
    unsafe {
        let set = shutdown_sigset();
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// The {SIGINT, SIGTERM} set rapira blocks and waits on.
unsafe fn shutdown_sigset() -> libc::sigset_t {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
    }
    set
}

/// Block until a blocked SIGINT/SIGTERM is delivered, then return its number. No timeout: the
/// caller waits on the signal like a channel receive. `sigwait` is portable across macOS,
/// Linux, and BSD, unlike `sigtimedwait`, which Darwin lacks.
///
/// https://man7.org/linux/man-pages/man2/sigwaitinfo.2.html
fn wait_shutdown_signal() -> libc::c_int {
    // SAFETY: `set` and `sig` are stack values live for the whole call.
    unsafe {
        let set = shutdown_sigset();
        let mut sig: libc::c_int = 0;
        libc::sigwait(&set, &mut sig);
        sig
    }
}

/// Spawn the shutdown watcher for this platform. It flips `stop_tx` — the same watch channel
/// every `drive` future selects on — so an external terminate/interrupt becomes a graceful
/// drain. The returned guard tears the watcher down on drop.
fn spawn_shutdown_watcher(stop_tx: watch::Sender<bool>) -> ShutdownWatcher {
    // Detached: block on the first signal to drain, on the second to force exit. If the
    // extensions finish on their own the thread stays parked in `sigwait` and is reclaimed
    // at process exit.
    std::thread::Builder::new()
        .name("rapira-signal".into())
        .spawn(move || {
            let _ = wait_shutdown_signal();
            log::info!(target: "rapira", "shutdown signal received; draining extensions");
            let _ = stop_tx.send(true);
            let _ = wait_shutdown_signal();
            log::warn!(target: "rapira", "second shutdown signal; forcing exit");
            std::process::exit(130);
        })
        .expect("spawn signal thread");
    ShutdownWatcher
}

struct ShutdownWatcher; // detached thread — nothing to unwind

/// Drives the extension tasks. `serve` also stops them on a signal; `join` only waits;
/// `drop` is the safety net.
pub struct Running {
    rt: Runtime,
    tasks: JoinSet<Outcome>,
    stop_tx: watch::Sender<bool>,
}

impl Running {
    /// rapira_core's entry: run until every extension finishes on its own OR a
    /// terminate/interrupt arrives; on shutdown, ask every extension to stop and
    /// drain, then return their outcomes so `main` can exit.
    pub fn serve(mut self) -> Vec<Outcome> {
        let _watcher = spawn_shutdown_watcher(self.stop_tx.clone());
        self.drain_all()
        // `_watcher` drops here; the detached reaper thread is reclaimed at process exit.
    }

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
    fn extension_host_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ExtensionHost>();
    }

    /// The reaper dequeues a blocked, pending signal via `sigwait` instead of letting it
    /// run the default (terminate) action — the basis of graceful shutdown.
    #[test]
    fn sigwait_reaps_a_blocked_signal() {
        arm_shutdown_signals();
        // SAFETY: SIGTERM is blocked in this thread, so `raise` leaves it pending here for
        // `sigwait` to dequeue; it never reaches the default (terminate) handler.
        unsafe { libc::raise(libc::SIGTERM) };
        assert_eq!(wait_shutdown_signal(), libc::SIGTERM);
    }
}
