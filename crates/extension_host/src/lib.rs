//! Registers native rapira extensions and drives each on a shared runtime. `serve`
//! runs until every extension finishes or a terminate/interrupt signal arrives; on a
//! signal it stops and drains them all — this is how rapira_core shuts down and exits.

use extension_api::{Extension, Php};
use php_sys::RapiraHandle;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

    /// Construct `E` (via `init`) and stage it. A duplicate name is a hard error;
    /// identity is captured for logging before `E` is moved into the launcher.
    pub fn register<E: Extension>(&mut self) -> anyhow::Result<()> {
        let ext = E::init();
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

    /// Spawn every extension on a shared runtime; one `Php` (the single `--script`) is
    /// cloned to each. The returned guard drives them to completion / shutdown.
    pub fn run(self, rapira: RapiraHandle, script: PathBuf) -> Running {
        self.run_with_grace(rapira, script, Duration::from_secs(30))
    }

    /// As [`run`](Self::run), with a custom per-extension graceful-shutdown budget.
    pub fn run_with_grace(self, rapira: RapiraHandle, script: PathBuf, grace: Duration) -> Running {
        let php = Php::new(rapira, script);
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
                        Ok(()) => log::info!("[ext {name}] finished"),
                        Err(msg) => log::error!("[ext {name}] {msg}"),
                    }
                    outcome
                },
                rt.handle(),
            );
        }

        Running { rt, tasks, stop_tx }
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
            // Resolve when the stop flag flips true (or the sender is dropped).
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

/// Block SIGINT/SIGTERM in the calling thread so every thread spawned afterwards (the
/// PHP workers, the extension runtime) inherits the block. rapira then reaps them with
/// `sigwait` on a dedicated thread instead of a signal handler — which is what lets it
/// coexist with Zend's per-request signal management: no `sigaction` disposition is ever
/// replaced, so PHP keeps its own deferred handlers and emits no "handler was replaced"
/// warning. Call once, in `main`, before booting PHP.
pub fn arm_shutdown_signals() {
    // SAFETY: operates on a stack-owned, freshly-initialized signal set.
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

/// Wait up to `timeout` for a blocked SIGINT/SIGTERM. `true` on receipt, `false` on
/// timeout — so the waiter can re-check whether the extensions already finished.
fn wait_shutdown_signal(timeout: Duration) -> bool {
    // SAFETY: every pointer references a stack value live for the whole call.
    unsafe {
        let set: libc::sigset_t = shutdown_sigset();
        let ts: libc::timespec = libc::timespec {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_nsec: timeout.subsec_nanos() as libc::c_long,
        };
        let mut info: libc::siginfo_t = std::mem::zeroed();
        libc::sigtimedwait(&set, &mut info, &ts) >= 0
    }
}

/// Drives the extension tasks. `serve` also stops them on a signal; `join` only waits;
/// `drop` is the safety net.
pub struct Running {
    rt: Runtime,
    tasks: JoinSet<Outcome>,
    stop_tx: watch::Sender<bool>,
}

impl Running {
    /// rapira_core's entry: run until every extension finishes on its own OR a
    /// terminate/interrupt signal arrives; on signal, ask every extension to shut down
    /// and drain, then return their outcomes so `main` can exit.
    pub fn serve(mut self) -> Vec<Outcome> {
        // A dedicated thread reaps SIGINT/SIGTERM via `sigwait` (blocked process-wide by
        // `arm_shutdown_signals`) and asks every extension to stop. It polls with a
        // timeout so it can exit once the extensions finish on their own; a second signal
        // during the drain forces an immediate exit so an operator can bail out.
        let done = Arc::new(AtomicBool::new(false));
        let signals = {
            let done = done.clone();
            let stop_tx = self.stop_tx.clone();
            std::thread::Builder::new()
                .name("rapira-signal".into())
                .spawn(move || {
                    let mut asked = false;
                    while !done.load(Ordering::Relaxed) {
                        if wait_shutdown_signal(Duration::from_millis(200)) {
                            if asked {
                                log::warn!("[rapira] second shutdown signal; forcing exit");
                                std::process::exit(130);
                            }
                            asked = true;
                            log::info!("[rapira] shutdown signal received; draining extensions");
                            let _ = stop_tx.send(true);
                        }
                    }
                })
                .expect("spawn signal thread")
        };
        let outcomes = self.drain_all();
        done.store(true, Ordering::Relaxed);
        let _ = signals.join();
        outcomes
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
        assert!(
            !wait_shutdown_signal(Duration::from_millis(10)),
            "no signal is pending yet"
        );
        // SAFETY: SIGTERM is blocked in this thread, so `raise` leaves it pending here
        // for `sigwait` to dequeue; it never reaches the default handler.
        unsafe { libc::raise(libc::SIGTERM) };
        assert!(
            wait_shutdown_signal(Duration::from_secs(1)),
            "the pending SIGTERM is reaped"
        );
    }
}
