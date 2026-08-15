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

pub mod multipart;

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
        self.run_with_options(rapira, script, RuntimeOptions::default())
    }

    /// As [`run`](Self::run), with the backend's request-shaping options.
    pub fn run_with_options(
        self,
        rapira: RapiraHandle,
        script: PathBuf,
        opts: RuntimeOptions,
    ) -> Running {
        let grace = opts.grace;
        let php = Php::new(
            Arc::new(RapiraBackend::new(rapira, script.clone(), opts)),
            script,
        );
        let (stop_tx, stop_rx) = watch::channel(false);
        let rt = tokio::runtime::Builder::new_multi_thread()
            // One thread: this runtime only drives `drive`'s shutdown timeout, and
            // extensions bring their own (the HTTP front runs on its own thread with
            // its own runtime). The default sizes to the CPU count, in every worker
            // process, so the pool multiplies it.
            .worker_threads(1)
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
                        Ok(()) => tracing::info!(target: "ext", "{name} finished"),
                        Err(msg) => tracing::error!(target: "ext", "{name}: {msg}"),
                    }
                    outcome
                },
                rt.handle(),
            );
        }

        Running { rt, tasks, stop_tx }
    }
}

/// The backend's request-shaping options, threaded from the binary's config.
pub struct RuntimeOptions {
    /// Per-extension graceful-shutdown budget.
    pub grace: Duration,
    /// Upload limits for host-parsed multipart; read only on a dispatcher
    /// handle (the worker/superglobals arm feeds php-src's own rfc1867
    /// through read_post).
    pub uploads: Arc<multipart::Limits>,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(30),
            uploads: Arc::new(multipart::Limits::default()),
        }
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
    /// Exchange-style delivery, read off the handle: one source of truth for
    /// the mode.
    dispatcher: bool,
    uploads: Arc<multipart::Limits>,
}

fn map_addr(a: extension_api::Addr) -> php_sys::types::Addr {
    match a {
        extension_api::Addr::Inet(sa) => php_sys::types::Addr::Inet(sa),
        extension_api::Addr::Unix(p) => php_sys::types::Addr::Unix(p),
    }
}

fn map_tls(t: extension_api::Tls) -> php_sys::types::TlsView {
    php_sys::types::TlsView {
        version: t.version,
        cipher: t.cipher,
        alpn: t.alpn,
        server_name: t.server_name,
        cert: t.cert.map(|c| php_sys::types::ClientCertView {
            serial: c.serial,
            organization: c.organization,
            fingerprint: c.fingerprint,
        }),
    }
}

fn parse_err(e: multipart::ParseError) -> anyhow::Error {
    match e {
        // downcastable: the extension answers 400/413 in its own protocol
        multipart::ParseError::Rejected(r) => anyhow::Error::new(r),
        // a host fault (ENOSPC, EACCES…), not the client's — the extension
        // finds the io::Error in the chain and answers 500
        multipart::ParseError::Io(io) => anyhow::Error::new(io).context("upload spool failed"),
    }
}

impl RapiraBackend {
    fn new(rapira: RapiraHandle, filename: PathBuf, opts: RuntimeOptions) -> Self {
        let document_root = filename
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let script_name = filename
            .file_name()
            .map_or_else(|| "/".to_string(), |f| format!("/{}", f.to_string_lossy()));
        let dispatcher = rapira.dispatcher();
        Self {
            rapira,
            filename,
            document_root,
            script_name,
            dispatcher,
            uploads: opts.uploads,
        }
    }

    /// The one `extension_api::Request → php_sys::Request` mapping. Multipart
    /// parses here, pre-enqueue: a rejected body is never dispatched and never
    /// touches the pending/active counters.
    async fn to_request(
        &self,
        mut req: extension_api::Request,
    ) -> anyhow::Result<php_sys::Request> {
        let query = req.uri.split_once('?').map_or("", |(_, q)| q).to_string();
        // Carried as raw bytes, like every other header value: the multipart
        // boundary comes out of this verbatim, so a lossy decode would turn a
        // non-UTF-8 boundary into U+FFFD and the body's real boundary would
        // never match.
        let content_type = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone());
        // wire byte count, captured before any body move
        let content_length = req.body.len() as i64;

        // Content-type is a singleton field (RFC 9110 §8.3,
        // https://www.rfc-editor.org/rfc/rfc9110#section-8.3): with a multipart
        // line anywhere among repeats, the host and a PHP consumer could parse
        // the body by different boundaries — whichever line comes first.
        if self.dispatcher && !req.body.is_empty() {
            let mut ct_lines = 0usize;
            let mut any_multipart = false;
            for (k, v) in &req.headers {
                if k.eq_ignore_ascii_case("content-type") {
                    ct_lines += 1;
                    any_multipart = any_multipart || multipart::is_multipart(v);
                }
            }
            if ct_lines > 1 && any_multipart {
                return Err(anyhow::Error::new(extension_api::Rejected {
                    status: 400,
                    reason: "repeated content-type field lines with a multipart body".into(),
                }));
            }
        }

        let body = if self.dispatcher
            && !req.body.is_empty()
            && let Some(ct) = content_type.as_deref()
            && multipart::is_multipart(ct)
        {
            let boundary = multipart::boundary(ct).map_err(parse_err)?;
            let bytes = std::mem::take(&mut req.body);
            let limits = Arc::clone(&self.uploads);
            // spool writes are file IO — off the reactor
            let parsed =
                tokio::task::spawn_blocking(move || multipart::parse(&bytes, &boundary, &limits))
                    .await
                    .map_err(|e| anyhow::anyhow!("multipart parse task failed: {e}"))?;
            php_sys::types::Body::Multipart(parsed.map_err(parse_err)?)
        } else {
            php_sys::types::Body::Raw(Box::new(Cursor::new(std::mem::take(&mut req.body))))
        };

        Ok(php_sys::Request {
            method: req.method,
            https: req.https,
            query,
            protocol: req.protocol,
            // normalized once at the producer: empty means "named none"
            target: req.target.filter(|t| !t.is_empty()),
            authority: req.authority.filter(|a| !a.is_empty()),
            remote: map_addr(req.remote),
            server: map_addr(req.server),
            server_name: req.server_name,
            server_port: req.server_port,
            script_name: self.script_name.clone(),
            document_root: self.document_root.clone(),
            script_filename: self.filename.clone(),
            content_type,
            content_length,
            body,
            headers: req.headers,
            server_vars: Vec::new(),
            uri: req.uri,
            received_at: req.received_at,
            tls: req.tls.map(map_tls),
        })
    }
}

impl extension_api::Backend for RapiraBackend {
    /// Submit `req`; the Reply wraps the frame receiver directly, so dropping
    /// it is the client-gone signal the exchange layer observes.
    fn exec(
        &self,
        req: extension_api::Request,
    ) -> Pin<Box<dyn Future<Output = extension_api::Result<extension_api::Reply>> + Send + '_>>
    {
        Box::pin(async move {
            // parse/reject happens before handle()'s pending increment — a
            // rejected request never touches the counters or the queue
            let req = self.to_request(req).await?;
            // shedding is this host's answer, not a gateway fault: 503 for a
            // saturated intake, 500 for a pool that is gone
            let rx = self.rapira.handle(req).await.map_err(|e| {
                anyhow::Error::new(extension_api::Rejected {
                    status: match e {
                        php_sys::HandleError::Saturated => 503,
                        php_sys::HandleError::Stopped => 500,
                    },
                    reason: e.to_string(),
                })
            })?;
            Ok(extension_api::Reply::new(Box::new(FrameSource(rx))))
        })
    }
}

/// `php_sys::Frame` receiver as a [`extension_api::ReplySource`]; the mapping
/// is field-for-field.
struct FrameSource(tokio::sync::mpsc::Receiver<php_sys::Frame>);

impl extension_api::ReplySource for FrameSource {
    fn next(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Option<extension_api::ReplyEvent>> + Send + '_>> {
        Box::pin(async move {
            self.0.recv().await.map(|frame| match frame {
                php_sys::Frame::Interim(h) => extension_api::ReplyEvent::Interim {
                    status: h.status,
                    headers: h.headers,
                },
                php_sys::Frame::Head {
                    head,
                    content_length,
                    bodiless,
                    body_coded,
                } => extension_api::ReplyEvent::Head {
                    status: head.status,
                    headers: head.headers,
                    content_length,
                    bodiless,
                    body_coded,
                },
                php_sys::Frame::Chunk(b) => extension_api::ReplyEvent::Chunk(b),
                php_sys::Frame::File { file, offset, len } => {
                    extension_api::ReplyEvent::File { file, offset, len }
                }
                php_sys::Frame::End {
                    trailers,
                    truncated,
                } => extension_api::ReplyEvent::End {
                    trailers,
                    truncated,
                },
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
                tracing::info!(target: "rapira", "signal {sig} received; draining worker");
                let _ = stop_tx.send(true);
                let _ = wait_signal(&[libc::SIGQUIT, libc::SIGINT]);
                tracing::warn!(target: "rapira", "second signal; forcing worker exit");
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

    use extension_api::{Reply, ReplyEvent, ReplySource};

    struct VecSource(std::collections::VecDeque<ReplyEvent>);

    impl ReplySource for VecSource {
        fn next(&mut self) -> Pin<Box<dyn Future<Output = Option<ReplyEvent>> + Send + '_>> {
            let ev = self.0.pop_front();
            Box::pin(async move { ev })
        }
    }

    fn reply(events: Vec<ReplyEvent>) -> Reply {
        Reply::new(Box::new(VecSource(events.into())))
    }

    fn head() -> ReplyEvent {
        ReplyEvent::Head {
            status: 200,
            headers: vec![("x-a".into(), b"1".to_vec())],
            content_length: None,
            bodiless: false,
            body_coded: false,
        }
    }

    fn end(truncated: bool) -> ReplyEvent {
        ReplyEvent::End {
            trailers: Vec::new(),
            truncated,
        }
    }

    /// The four stream outcomes map to the three documented errors and Ok.
    #[tokio::test]
    async fn collect_maps_stream_outcomes() {
        let died = reply(Vec::new()).collect().await.unwrap_err();
        assert!(died.to_string().contains("died mid-response"), "{died:#}");

        let cut = reply(vec![head()]).collect().await.unwrap_err();
        assert!(cut.to_string().contains("truncated"), "{cut:#}");

        let cut = reply(vec![head(), end(true)]).collect().await.unwrap_err();
        assert!(cut.to_string().contains("truncated"), "{cut:#}");

        let headless = reply(vec![end(false)]).collect().await.unwrap_err();
        assert!(
            headless.to_string().contains("no response head"),
            "{headless:#}"
        );
    }

    /// Chunks concatenate in order; interim heads are dropped.
    #[tokio::test]
    async fn collect_concatenates_the_stream() {
        let r = reply(vec![
            ReplyEvent::Interim {
                status: 103,
                headers: Vec::new(),
            },
            head(),
            ReplyEvent::Chunk(bytes::Bytes::from_static(b"one,")),
            ReplyEvent::Chunk(bytes::Bytes::from_static(b"two")),
            end(false),
        ])
        .collect()
        .await
        .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.headers, vec![("x-a".to_string(), b"1".to_vec())]);
        assert_eq!(r.body, b"one,two");
    }

    /// The pingora front classifies a spool failure by finding an `io::Error`
    /// in the chain (500, host fault) and a client fault by downcasting
    /// `Rejected` — parse_err must keep both typed, never stringified.
    #[test]
    fn parse_err_keeps_the_typed_causes() {
        let io = parse_err(multipart::ParseError::Io(std::io::Error::other(
            "disk full",
        )));
        assert!(io.chain().any(|c| c.is::<std::io::Error>()));
        assert!(io.downcast_ref::<extension_api::Rejected>().is_none());

        let rejected = parse_err(multipart::ParseError::Rejected(extension_api::Rejected {
            status: 413,
            reason: "too big".into(),
        }));
        assert_eq!(
            rejected
                .downcast_ref::<extension_api::Rejected>()
                .map(|r| r.status),
            Some(413)
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
