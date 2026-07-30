//! E2E harness: spawn a real `rapira serve` master, drive it over HTTP and
//! signals, and observe the worker pool through `ps`. Every wait is bounded and
//! every failure dumps `ps` output plus the server-log tail.

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Connect budget for a freshly spawned master (CI macOS worst case).
pub const BOOT: Duration = Duration::from_secs(30);

// Frozen master exit codes; OK and FAILBOOT are asserted, FORCED names diagnostics.
/// Master could not bring up a serviceable gen-0 pool.
pub const MASTER_EXIT_FAILBOOT: i32 = 70;
/// Master graceful stop completed.
pub const MASTER_EXIT_OK: i32 = 0;
/// Master forced stop (a second signal arrived while draining).
pub const MASTER_EXIT_FORCED: i32 = 130;

/// A running master and the scratch dir holding its config and log.
pub struct Server {
    pub child: Child,
    pub addr: SocketAddr,
    pub dir: PathBuf,
}

impl Server {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Poll `try_wait` until the master exits or `timeout` elapses.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let end = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(st)) => return Some(st),
                Ok(None) => {}
                Err(_) => return None,
            }
            if Instant::now() >= end {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Non-blocking exit check.
    pub fn try_status(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Only signal a live master: the pid of a reaped child may be reused.
        if self.child.try_wait().ok().flatten().is_none() {
            signal(self.child.id(), libc::SIGTERM);
            if self.wait_exit(Duration::from_secs(5)).is_none() {
                // Query workers only now: a snapshot from before the SIGTERM
                // could name pids that exited (and were reused) meanwhile.
                let kids = worker_pids(self.child.id());
                signal(self.child.id(), libc::SIGKILL);
                for k in kids {
                    signal(k, libc::SIGKILL);
                }
                let _ = self.child.wait();
            }
        }
        if std::thread::panicking() {
            eprintln!("{}", log_tail(&self.dir));
            return;
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Path to the built `rapira` binary. The bin is defined by the root package,
/// so CARGO_BIN_EXE is not set here — locate it beside the test binary
/// (target/<profile>/deps/<test> -> target/<profile>/rapira). `RAPIRA_BIN`
/// overrides. The Makefile/CI build the bin before running this suite.
fn rapira_bin() -> PathBuf {
    if let Ok(p) = std::env::var("RAPIRA_BIN") {
        return PathBuf::from(p);
    }
    let exe = std::env::current_exe().expect("current_exe");
    let bin = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile> dir")
        .join("rapira");
    assert!(
        bin.exists(),
        "rapira binary not found at {}; build it first (cargo build -p rapira_core --bin rapira) or set RAPIRA_BIN",
        bin.display()
    );
    bin
}

/// Boot a master with a generated config, retrying the ephemeral port up to 3
/// times to survive the bind :0 -> spawn TOCTOU race. Panics with the log tail
/// if the master never accepts a connection.
///
/// `extra_toml` is appended inside the `[pool]` table, so bare keys are pool
/// keys. Any other section (`[supervisor]`, `[log]`) needs its own header and
/// must come after all bare pool keys — a header closes `[pool]` for everything
/// that follows. A misplaced key is a boot error that surfaces here as the
/// generic "never accepted a connection" panic.
pub fn spawn_with_config(fixture: &str, processes: usize, extra_toml: &str) -> Server {
    spawn_with_extras(fixture, processes, "", extra_toml, Some("info"))
}

/// [`spawn_with_config`] for keys that belong inside the `[http]` table, which the
/// trailing `extra_toml` cannot reach without redeclaring the table.
pub fn spawn_with_http_extra(fixture: &str, processes: usize, http_extra: &str) -> Server {
    spawn_with_extras(fixture, processes, http_extra, "", Some("info"))
}

/// [`spawn_with_config`] without the pinned `RUST_LOG`, so the `[log]` section
/// owns the filter — for tests asserting config-driven filtering.
pub fn spawn_without_rust_log(fixture: &str, processes: usize, extra_toml: &str) -> Server {
    spawn_with_extras(fixture, processes, "", extra_toml, None)
}

fn spawn_with_extras(
    fixture: &str,
    processes: usize,
    http_extra: &str,
    extra_toml: &str,
    rust_log: Option<&str>,
) -> Server {
    let dir = scratch_dir();
    std::fs::copy(fixture_path(fixture), dir.join(fixture))
        .unwrap_or_else(|e| panic!("copy fixture {fixture}: {e}"));
    let mut last_log = String::new();
    for _ in 0..3 {
        let port = free_port();
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        std::fs::write(
            dir.join("rapira.toml"),
            render_config(port, processes, fixture, http_extra, extra_toml),
        )
        .expect("write config");
        let log = File::create(dir.join("server.log")).expect("create server.log");
        let mut cmd = Command::new(rapira_bin());
        cmd.args(["serve", "--config"]).arg(dir.join("rapira.toml"));
        match rust_log {
            // Pinned by default so worker/scaling activity lands in the failure
            // diagnostics regardless of the config under test.
            Some(v) => cmd.env("RUST_LOG", v),
            // Cleared, not just unpinned: the developer's shell may set it.
            None => cmd.env_remove("RUST_LOG"),
        };
        let mut child = cmd
            .stdout(Stdio::from(log.try_clone().expect("clone log fd")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn rapira");
        if wait_for_port(&addr, &mut child, BOOT) {
            return Server { child, addr, dir };
        }
        // Bind race or boot failure: reap and retry on a fresh port.
        let _ = child.kill();
        let _ = child.wait();
        last_log = log_tail(&dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
    panic!("rapira never accepted a connection after 3 attempts\n{last_log}");
}

fn render_config(
    port: u16,
    processes: usize,
    fixture: &str,
    http_extra: &str,
    extra: &str,
) -> String {
    format!(
        "[http]\nlisten = \"127.0.0.1:{port}\"\n{http_extra}\n\
         [pool]\nprocesses = {processes}\nentrypoint = \"{fixture}\"\n\n\
         {extra}"
    )
}

/// Connect-only readiness: the master binds the listen socket before forking, so
/// a successful connect means "boot far enough to serve". Returns false early if
/// the child exits first.
fn wait_for_port(addr: &SocketAddr, child: &mut Child, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if TcpStream::connect_timeout(addr, Duration::from_millis(200)).is_ok() {
            // The connect could have reached a free_port() collision winner, not
            // our child. Give a bind failure a moment to surface, then require
            // the child alive; false falls through to the caller's retry loop.
            std::thread::sleep(Duration::from_millis(100));
            return child.try_wait().ok().flatten().is_none();
        }
        if child.try_wait().ok().flatten().is_some() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Hand-rolled HTTP/1.1 GET with `Connection: close`; the body is read to EOF
/// (close-delimited), so no chunked/keep-alive parsing is needed.
pub fn http_get(addr: SocketAddr, path: &str, timeout: Duration) -> io::Result<(u16, Vec<u8>)> {
    http_get_with_headers(addr, path, &[], timeout)
}

/// [`http_get`] plus extra request fields, written in the order given so a repeated
/// name stays repeated on the wire.
pub fn http_get_with_headers(
    addr: SocketAddr,
    path: &str,
    fields: &[(&str, &str)],
    timeout: Duration,
) -> io::Result<(u16, Vec<u8>)> {
    parse_status_and_body(&http_get_raw(addr, path, fields, timeout)?)
}

/// The whole response, head included — for assertions about which fields actually
/// reached the client.
pub fn http_get_raw(
    addr: SocketAddr,
    path: &str,
    fields: &[(&str, &str)],
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let mut s = TcpStream::connect_timeout(&addr, timeout)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n"
    )?;
    for (name, value) in fields {
        write!(s, "{name}: {value}\r\n")?;
    }
    write!(s, "\r\n")?;
    s.flush()?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    Ok(raw)
}

/// Sibling of [`http_get`] with a body. `content_type` is bytes, not text: a multipart
/// boundary is opaque octets and obs-text is legal in a field value.
pub fn http_post(
    addr: SocketAddr,
    path: &str,
    content_type: &[u8],
    body: &[u8],
    timeout: Duration,
) -> io::Result<(u16, Vec<u8>)> {
    let mut s = TcpStream::connect_timeout(&addr, timeout)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    let mut req = Vec::new();
    write!(
        req,
        "POST {path} HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n"
    )?;
    req.extend_from_slice(b"Content-Type: ");
    req.extend_from_slice(content_type);
    write!(req, "\r\nContent-Length: {}\r\n\r\n", body.len())?;
    req.extend_from_slice(body);
    s.write_all(&req)?;
    s.flush()?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    parse_status_and_body(&raw)
}

fn parse_status_and_body(raw: &[u8]) -> io::Result<(u16, Vec<u8>)> {
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header terminator"))?;
    let status_end = raw
        .windows(2)
        .position(|w| w == b"\r\n")
        .unwrap_or(head_end);
    let status_line = std::str::from_utf8(&raw[..status_end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 status line"))?;
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no status code"))?;
    Ok((code, raw[head_end + 4..].to_vec()))
}

/// Direct, non-zombie children of `master`. `ps -axo pid=,ppid=,state=` is valid
/// on both procps (Linux) and BSD ps (macOS); a `Z` state would count a
/// dead-but-unreaped worker during a respawn/reload window, so it is excluded.
pub fn worker_pids(master: u32) -> Vec<u32> {
    let out = match Command::new("ps")
        .args(["-axo", "pid=,ppid=,state="])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut pids = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(state)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        if ppid == master && !state.starts_with('Z') {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids
}

/// Poll `worker_pids` at 50ms until `pred` holds; panic with diagnostics on
/// deadline. Returns the matching pid set.
pub fn wait_workers(
    srv: &Server,
    deadline: Duration,
    what: &str,
    pred: impl Fn(&[u32]) -> bool,
) -> Vec<u32> {
    let master = srv.child.id();
    let end = Instant::now() + deadline;
    loop {
        let pids = worker_pids(master);
        if pred(&pids) {
            return pids;
        }
        if Instant::now() >= end {
            panic!(
                "timed out after {deadline:?} waiting for {what}\n{}",
                diagnostics(srv)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn signal(pid: u32, sig: i32) {
    // SAFETY: kill is a plain syscall; a stale pid returns ESRCH, which we ignore.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

/// Per-thread request outcome counters. `refused` = connection never accepted
/// (the listener closed); the reload keeps it open, so it must stay 0.
/// `truncated` = accepted then reset mid-response, the prefork accept-race when
/// a recycled worker drops a just-accepted connection; bounded by workers cycled.
pub struct Tally {
    pub ok: u64,
    pub refused: u64,
    pub truncated: u64,
    pub last_err: Option<String>,
}

impl Tally {
    fn new() -> Tally {
        Tally {
            ok: 0,
            refused: 0,
            truncated: 0,
            last_err: None,
        }
    }
}

/// A pool of threads hammering the server until [`Storm::halt`].
pub struct Storm {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<Tally>>,
}

/// Launch `threads` workers, each looping `GET /` until halted. 200 counts ok,
/// anything else counts failed and records the last error.
pub fn storm(addr: SocketAddr, threads: usize) -> Storm {
    let stop = Arc::new(AtomicBool::new(false));
    let handles = (0..threads)
        .map(|_| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut tally = Tally::new();
                while !stop.load(Ordering::Relaxed) {
                    match http_get(addr, "/", Duration::from_secs(10)) {
                        Ok((200, _)) => tally.ok += 1,
                        Ok((code, _)) => {
                            tally.truncated += 1;
                            tally.last_err = Some(format!("status {code}"));
                        }
                        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                            tally.refused += 1;
                            tally.last_err = Some(e.to_string());
                        }
                        Err(e) => {
                            tally.truncated += 1;
                            tally.last_err = Some(e.to_string());
                        }
                    }
                }
                tally
            })
        })
        .collect();
    Storm {
        stop,
        threads: handles,
    }
}

impl Storm {
    pub fn halt(self) -> Tally {
        self.stop.store(true, Ordering::Relaxed);
        let mut total = Tally::new();
        for h in self.threads {
            if let Ok(t) = h.join() {
                total.ok += t.ok;
                total.refused += t.refused;
                total.truncated += t.truncated;
                if t.last_err.is_some() {
                    total.last_err = t.last_err;
                }
            }
        }
        total
    }
}

/// Assert the master exited with `expected`, naming both codes on mismatch.
pub fn assert_exit_code(status: Option<ExitStatus>, expected: i32, srv: &Server) {
    match status.and_then(|s| s.code()) {
        Some(code) if code == expected => {}
        Some(code) => panic!(
            "expected exit {expected} [{}], got {code} [{}]\n{}",
            code_name(expected),
            code_name(code),
            diagnostics(srv)
        ),
        None => panic!(
            "expected exit {expected} [{}], but the master was killed by a signal or is still running\n{}",
            code_name(expected),
            diagnostics(srv)
        ),
    }
}

fn code_name(code: i32) -> String {
    match code {
        MASTER_EXIT_OK => "DRAINED/OK".into(),
        MASTER_EXIT_FAILBOOT => "MASTER_FAILBOOT".into(),
        MASTER_EXIT_FORCED => "MASTER_FORCED".into(),
        other => format!("code {other}"),
    }
}

/// Worker pids + `ps` subtree + server-log tail, for failure messages.
pub fn diagnostics(srv: &Server) -> String {
    let master = srv.child.id();
    format!(
        "master pid {master}, workers {:?}\n{}\n{}",
        worker_pids(master),
        ps_snapshot(master),
        log_tail(&srv.dir)
    )
}

fn ps_snapshot(master: u32) -> String {
    let out = match Command::new("ps")
        .args(["-axo", "pid=,ppid=,state=,command"])
        .output()
    {
        Ok(o) => o,
        Err(e) => return format!("ps failed: {e}"),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = vec![format!("--- ps (master {master} subtree) ---")];
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        if pid == master || ppid == master {
            lines.push(line.trim().to_owned());
        }
    }
    lines.join("\n")
}

fn log_tail(dir: &Path) -> String {
    let path = dir.join("server.log");
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let tail: Vec<&str> = content.lines().rev().take(40).collect();
    let body: String = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
    format!(
        "--- server.log tail ({}) ---\n{body}\n--- end ---",
        path.display()
    )
}

fn scratch_dir() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rapira-e2e-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/e2e/fixtures")
        .join(name)
}

fn free_port() -> u16 {
    let l = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    l.local_addr().expect("local_addr").port()
}
