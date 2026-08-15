//! Resolves rapira's runtime settings from three layers, in order of precedence:
//! CLI flags > `rapira.toml` > built-in defaults. Everything collapses into one
//! validated [`Settings`], the single struct `main` consumes. (Env vars are
//! intentionally absent here; the binary alone reads `RUST_LOG`, as a debugging
//! override of the `[log]` filter.)

use anyhow::{Context, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

/// A validated bind address. TCP for `host:port` / `:port`, Unix for `unix:<path>`.
/// Parsing lives in [`FromStr`]; [`Display`] round-trips back to that syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

impl fmt::Display for Listen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // `Tcp(0.0.0.0:8000)` prints as `0.0.0.0:8000`, not `:8000` — it binds the
            // same interfaces and re-parses to the same value.
            Listen::Tcp(addr) => write!(f, "{addr}"),
            Listen::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

/// A listen address failed to parse. Implements [`std::error::Error`] so clap's
/// derived value parser accepts `Option<Listen>` via this `FromStr`.
#[derive(Debug)]
pub struct ListenParseError(String);

impl fmt::Display for ListenParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ListenParseError {}

impl FromStr for Listen {
    type Err = ListenParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if let Some(path) = s.strip_prefix("unix:") {
            if path.is_empty() {
                return Err(ListenParseError("unix socket path is empty".into()));
            }
            return Ok(Listen::Unix(PathBuf::from(path)));
        }
        // A bare port ("8000") has no interface; both TCP forms carry a ':'.
        if !s.contains(':') {
            return Err(ListenParseError(format!(
                "`{s}` is not a listen address: use host:port, :port, or unix:<path>"
            )));
        }
        // `:port` → all interfaces. An IPv6 literal (`[::1]:8000`) has a ':' but never
        // leads with one, so it falls through to the SocketAddr parse below.
        if let Some(port) = s.strip_prefix(':') {
            let port: u16 = port
                .parse()
                .map_err(|_| ListenParseError(format!("`{s}` has an invalid port")))?;
            return Ok(Listen::Tcp(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))));
        }
        s.parse::<SocketAddr>().map(Listen::Tcp).map_err(|_| {
            ListenParseError(format!(
                "`{s}` is not host:port (expected an IP literal, e.g. 127.0.0.1:8000)"
            ))
        })
    }
}

/// CLI-supplied overrides, layered on top of the config file. Plain data — clap
/// builds this in `main`; `None`/`false` means "not overridden here".
#[derive(Debug, Default)]
pub struct Overrides {
    pub listen: Option<Listen>,
    pub processes: Option<usize>,
    /// `--mode` (or the `--classic` alias); `Some` overrides the file's `pool.mode`.
    pub mode: Option<RunMode>,
    /// Positional `SCRIPT`; overrides `pool.entrypoint`.
    pub entrypoint: Option<PathBuf>,
}

/// The one validated settings struct the server boots from.
#[derive(Debug)]
pub struct Settings {
    pub http: HttpSettings,
    pub pool: PoolSettings,
    pub supervisor: SupervisorSettings,
    pub log: LogSettings,
}

/// How a pool scales its worker processes (`pool.scaling`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scaling {
    /// Fixed pool of `pool.processes` workers.
    Static,
    /// Scale between spare-capacity thresholds, capped by `pool.processes`.
    Dynamic { min_spare: usize, max_spare: usize },
    /// Fork on demand, up to `pool.processes`; idle workers exit after
    /// `process_idle_timeout`.
    Ondemand,
}

/// Which run mode the pool's workers execute (`pool.mode`).
// No deny_unknown_fields: it governs struct variants, so it is inert on a unit-only enum.
// An unrecognised value is already an error because serde has no variant to match it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    /// Re-include the entry script for every request.
    Classic,
    /// Resident script; the host hands each request to a PHP handler closure.
    Worker,
    /// Resident script driving the dispatcher/exchange surface.
    #[default]
    Dispatcher,
}

impl RunMode {
    /// The config-vocabulary name, as `pool.mode` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            RunMode::Classic => "classic",
            RunMode::Worker => "worker",
            RunMode::Dispatcher => "dispatcher",
        }
    }
}

// clap derives its value parser from FromStr, so the CLI shares the config vocabulary.
impl std::str::FromStr for RunMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "classic" => Ok(RunMode::Classic),
            "worker" => Ok(RunMode::Worker),
            "dispatcher" => Ok(RunMode::Dispatcher),
            other => Err(format!(
                "unknown mode `{other}` (expected classic, worker, or dispatcher)"
            )),
        }
    }
}

/// Master-scoped supervision: pid identity on disk and the stop-escalation
/// budget. Nothing here is per-pool.
#[derive(Debug)]
pub struct SupervisorSettings {
    /// Graceful-stop budget before the master escalates QUIT → TERM → KILL.
    pub process_control_timeout: Duration,
    /// Master pidfile; relative paths resolve against the config file's directory.
    pub pidfile: Option<PathBuf>,
}

impl SupervisorSettings {
    /// How long a worker may drain in-flight work before the master escalates.
    /// The master sends QUIT at t=0 and SIGTERM at `process_control_timeout`, and
    /// the fork bracket deliberately leaves SIGTERM at SIG_DFL in the child so
    /// that escalation is a fast kill — a drain still running at the deadline is
    /// therefore cut short, mid-response. Subtracting the margin gives the worker
    /// room to finish, or to report what it stranded, before that happens.
    pub fn drain_grace(&self) -> Duration {
        /// Headroom between the end of a drain and the master's escalation.
        const MARGIN: Duration = Duration::from_secs(5);
        let margin = MARGIN.min(self.process_control_timeout / 2);
        self.process_control_timeout - margin
    }
}

/// Verbosity of a log target.
// No deny_unknown_fields: it governs struct variants, so it is inert on a unit-only enum.
// An unrecognised value is already an error because serde has no variant to match it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    #[default]
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// The config-vocabulary name, as a `RUST_LOG`-style filter directive expects it.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

/// Output shape of a log record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Plain,
    Json,
}

#[derive(Debug)]
pub struct LogSettings {
    pub level: LogLevel,
    pub format: LogFormat,
    /// Per-target overrides. Keys match by prefix (`php` also covers
    /// `php_sys::...`). BTreeMap so the rendered filter is byte-stable.
    pub targets: BTreeMap<String, LogLevel>,
}

#[derive(Debug)]
pub struct HttpSettings {
    pub listen: Listen,
    pub server_name: String,
    /// What PHP sees as SERVER_PORT; defaults to the listen TCP port (80 for unix:).
    pub server_port: u16,
    /// Bytes, converted from the config's `max_body_size_mb`.
    pub max_body_size: usize,
    /// Per-write bound on the response path, from `write_timeout_secs`.
    pub write_timeout: std::time::Duration,
    pub unsafe_field_names: UnsafeFieldNames,
    pub uploads: UploadSettings,
    /// `[http.sendfile].root`; None = the entrypoint's directory.
    pub sendfile_root: Option<PathBuf>,
}

/// Multipart limits and the spool root (`[http.uploads]`); past a limit the
/// host answers 413 before dispatch.
#[derive(Debug)]
pub struct UploadSettings {
    /// Spool root for file parts; workers spool into a per-pid subdirectory.
    pub dir: PathBuf,
    /// Bytes, from `max_file_size_mb`.
    pub max_file_size: u64,
    /// Bytes, from `max_field_size_kb`.
    pub max_field_size: usize,
    pub max_files: usize,
    pub max_parts: usize,
    pub max_part_headers: usize,
}

/// What to do with a request field whose name reaches a CGI variable another name owns:
/// the `HTTP_*` mapping rewrites `-` to `_`, and PHP rewrites `.` to `_` again, so
/// `X_Forwarded_For` and `X.Forwarded.For` both land on `HTTP_X_FORWARDED_FOR`.
/// Only `[A-Za-z0-9-]` is safe.
///
/// There is deliberately no "allow" arm: an off-switch would re-open the very collision the
/// screen exists to close, and a client that can pick the aliasing spelling can overwrite a
/// field the front set. An allowlist of specific expected names could be safe; a boolean
/// could not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
// No deny_unknown_fields: it governs struct variants, so it is inert on a unit-only enum.
// An unrecognised value is already an error because serde has no variant to match it.
#[serde(rename_all = "lowercase")]
pub enum UnsafeFieldNames {
    /// Remove the field before PHP sees it.
    #[default]
    Drop,
    /// Answer 400 and serve nothing.
    Reject,
}

/// One resolved worker pool: what to run, how many, and the per-pool recycle
/// policy. The runtime is single-pool today; the shape is per-pool so a plugin
/// section can own one.
#[derive(Debug)]
pub struct PoolSettings {
    /// Absolute path to the PHP entry script.
    pub entrypoint: PathBuf,
    pub processes: usize,
    pub mode: RunMode,
    pub scaling: Scaling,
    /// Requests a worker serves before recycling itself (with jitter); 0 = unlimited.
    pub max_requests: u64,
    /// Ondemand: idle worker lifetime before the master retires it.
    pub process_idle_timeout: Duration,
    /// Wall-clock bound on a single request; a worker whose request runs longer
    /// is killed and replaced. Zero = disabled.
    pub request_terminate_timeout: Duration,
}

/// `rapira.toml` as written. Every field is optional so absence stays distinct from a
/// set value (needed for precedence). `deny_unknown_fields` at both levels turns a typo
/// like `[htttp]` or `lissten` into a hard error.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    http: HttpSection,
    #[serde(default)]
    pool: PoolSection,
    #[serde(default)]
    supervisor: SupervisorSection,
    #[serde(default)]
    log: LogSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpSection {
    listen: Option<String>,
    server_name: Option<String>,
    server_port: Option<u16>,
    max_body_size_mb: Option<usize>,
    write_timeout_secs: Option<u64>,
    /// Unrecognised values are a boot error, not a silent fall back to the default —
    /// a security knob that survives a typo is worse than one that refuses to start.
    unsafe_field_names: Option<UnsafeFieldNames>,
    /// Option so presence is observable: the table configures the host-side
    /// multipart parser, which only dispatcher mode runs.
    uploads: Option<UploadsSection>,
    #[serde(default)]
    sendfile: SendfileSection,
}

/// `[http.sendfile]`: the root `sendFile()` paths must stay inside.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendfileSection {
    root: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadsSection {
    dir: Option<String>,
    max_file_size_mb: Option<u64>,
    max_field_size_kb: Option<usize>,
    max_files: Option<usize>,
    max_parts: Option<usize>,
    max_part_headers: Option<usize>,
}

/// One pool table as written. Embedded by name — never `#[serde(flatten)]`, which
/// serde does not support alongside `deny_unknown_fields` — so a plugin table can
/// carry its own pool with typo denial intact.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PoolSection {
    entrypoint: Option<String>,
    processes: Option<usize>,
    mode: Option<RunMode>,
    scaling: Option<ScalingKey>,
    min_spare: Option<usize>,
    max_spare: Option<usize>,
    max_requests: Option<u64>,
    process_idle_timeout_secs: Option<u64>,
    request_terminate_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ScalingKey {
    Static,
    Dynamic,
    Ondemand,
}

/// Master-scoped supervision as written. Nothing here is per-pool.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SupervisorSection {
    pidfile: Option<String>,
    process_control_timeout_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogSection {
    level: Option<LogLevel>,
    format: Option<LogFormat>,
    /// Open-ended table: keys are log targets, so `deny_unknown_fields` cannot
    /// apply — key shape is validated in `resolve_log` instead.
    #[serde(default)]
    targets: BTreeMap<String, LogLevel>,
}

/// Default worker-process count: one per logical CPU. Falls back to 1 if the
/// platform can't report it.
fn default_processes() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn default_listen() -> Listen {
    Listen::Tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, 8000)))
}

/// Load `rapira.toml` (if given), merge CLI overrides on top, and validate. This is the
/// crate's whole public surface besides [`Listen`].
pub fn resolve(config_path: Option<&Path>, cli: Overrides) -> anyhow::Result<Settings> {
    let (file, config_dir) = match config_path {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading config file {}", path.display()))?;
            let file = load_str(&text)
                .with_context(|| format!("parsing config file {}", path.display()))?;
            (file, path.parent().map(Path::to_owned))
        }
        None => (FileConfig::default(), None),
    };
    merge(file, cli, config_dir.as_deref())
}

fn load_str(text: &str) -> anyhow::Result<FileConfig> {
    Ok(toml::from_str(text)?)
}

/// Apply precedence (CLI > file > default) and produce a validated [`Settings`]. Split
/// from [`resolve`] so precedence/validation are unit-testable without touching disk.
fn merge(file: FileConfig, cli: Overrides, config_dir: Option<&Path>) -> anyhow::Result<Settings> {
    let listen = match &cli.listen {
        Some(l) => l.clone(),
        None => match file.http.listen.as_deref() {
            Some(s) => s
                .parse::<Listen>()
                .with_context(|| format!("invalid http.listen `{s}`"))?,
            None => default_listen(),
        },
    };

    // SERVER_PORT should match what clients actually connect to, so an unset
    // server_port follows the listen port; unix sockets sit behind a proxy, where 80
    // is the conventional answer.
    let server_port = match file.http.server_port {
        Some(p) => p,
        None => match &listen {
            Listen::Tcp(addr) => addr.port(),
            Listen::Unix(_) => 80,
        },
    };

    let max_body_size_mb = file.http.max_body_size_mb.unwrap_or(8);
    if max_body_size_mb == 0 {
        bail!("http.max_body_size_mb must be at least 1");
    }
    let max_body_size = max_body_size_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow::anyhow!("http.max_body_size_mb {max_body_size_mb} is too large"))?;

    let write_timeout_secs = file.http.write_timeout_secs.unwrap_or(30);
    if write_timeout_secs == 0 {
        bail!("http.write_timeout_secs must be at least 1");
    }
    let write_timeout = capped_timeout("http", "write_timeout_secs", write_timeout_secs)?;

    let sendfile_root = match file.http.sendfile.root.filter(|r| !r.is_empty()) {
        Some(r) => Some(config_relative(config_dir, &r)?),
        None => None,
    };

    let pool = resolve_pool(file.pool, &cli, config_dir, "pool")?;
    // The table configures the host-side multipart parser; outside dispatcher
    // mode php-src parses the body and php.ini owns the limits, so an explicit
    // table would sit inert — refuse it instead.
    if file.http.uploads.is_some() && pool.mode != RunMode::Dispatcher {
        bail!(
            "http.uploads applies to dispatcher mode only (pool.mode = \"{}\")",
            pool.mode.as_str()
        );
    }
    let uploads = resolve_uploads(file.http.uploads.unwrap_or_default(), config_dir)?;
    let supervisor = resolve_supervisor(file.supervisor, config_dir)?;
    let log = resolve_log(file.log)?;

    Ok(Settings {
        http: HttpSettings {
            listen,
            server_name: file
                .http
                .server_name
                .unwrap_or_else(|| "localhost".to_owned()),
            server_port,
            max_body_size,
            write_timeout,
            unsafe_field_names: file.http.unsafe_field_names.unwrap_or_default(),
            uploads,
            sendfile_root,
        },
        pool,
        supervisor,
        log,
    })
}

/// Path/limit resolution only; the binary probes the directory at boot.
fn resolve_uploads(
    section: UploadsSection,
    config_dir: Option<&Path>,
) -> anyhow::Result<UploadSettings> {
    let dir = match section.dir.filter(|d| !d.is_empty()) {
        Some(d) => config_relative(config_dir, &d)?,
        None => std::env::temp_dir(),
    };
    let max_file_size_mb = section.max_file_size_mb.unwrap_or(2);
    if max_file_size_mb == 0 {
        bail!("http.uploads.max_file_size_mb must be at least 1");
    }
    let max_file_size = max_file_size_mb.checked_mul(1024 * 1024).ok_or_else(|| {
        anyhow::anyhow!("http.uploads.max_file_size_mb {max_file_size_mb} is too large")
    })?;
    let max_field_size_kb = section.max_field_size_kb.unwrap_or(256);
    if max_field_size_kb == 0 {
        bail!("http.uploads.max_field_size_kb must be at least 1");
    }
    let max_field_size = max_field_size_kb.checked_mul(1024).ok_or_else(|| {
        anyhow::anyhow!("http.uploads.max_field_size_kb {max_field_size_kb} is too large")
    })?;
    let max_parts = section.max_parts.unwrap_or(1024);
    if max_parts == 0 {
        bail!("http.uploads.max_parts must be at least 1");
    }
    let max_part_headers = section.max_part_headers.unwrap_or(32);
    if max_part_headers == 0 {
        bail!("http.uploads.max_part_headers must be at least 1");
    }
    let max_files = section.max_files.unwrap_or(20);
    if max_files == 0 {
        // zero would 413 every file part while the server boots clean
        bail!("http.uploads.max_files must be at least 1");
    }
    Ok(UploadSettings {
        dir,
        max_file_size,
        max_field_size,
        max_files,
        max_parts,
        max_part_headers,
    })
}

/// Resolve one pool table. `table` is the key path used in every error message
/// (`pool` today, `grpc.pool` when a plugin owns a pool). `cli` carries CLI
/// overrides, which apply to the root pool only — pass `&Overrides::default()`
/// for any other pool.
fn resolve_pool(
    section: PoolSection,
    cli: &Overrides,
    config_dir: Option<&Path>,
    table: &str,
) -> anyhow::Result<PoolSettings> {
    let processes = cli
        .processes
        .or(section.processes)
        .unwrap_or_else(default_processes);
    if processes == 0 {
        bail!("{table}.processes must be at least 1");
    }

    let mode = cli.mode.or(section.mode).unwrap_or_default();

    // Positional SCRIPT is cwd-relative; a config entrypoint is resolved against
    // the config file's directory so the config is relocatable.
    // `.filter` routes an empty `entrypoint = ""` to the clear bail below instead of
    // letting `base.join("")` silently resolve to the config directory.
    let entrypoint = if let Some(script) = &cli.entrypoint {
        std::path::absolute(script)?
    } else if let Some(ep) = section.entrypoint.as_deref().filter(|s| !s.is_empty()) {
        config_relative(config_dir, ep)?
    } else {
        // "pass a SCRIPT argument" is root-pool advice; generalize the message
        // when a plugin pool actually exists.
        bail!("no entrypoint: pass a SCRIPT argument or set {table}.entrypoint in the config file");
    };

    let scaling = match section.scaling.unwrap_or(ScalingKey::Static) {
        ScalingKey::Dynamic => {
            let (Some(min_spare), Some(max_spare)) = (section.min_spare, section.max_spare) else {
                bail!(
                    "{table}.scaling = \"dynamic\" requires {table}.min_spare and {table}.max_spare"
                );
            };
            if !(1..=max_spare).contains(&min_spare) || max_spare > processes {
                bail!(
                    "{table} spares must satisfy 1 <= min_spare ({min_spare}) <= max_spare ({max_spare}) <= {table}.processes ({processes})"
                );
            }
            Scaling::Dynamic {
                min_spare,
                max_spare,
            }
        }
        other => {
            // A spare key under static/ondemand is a scaling typo, not a tunable.
            if section.min_spare.is_some() || section.max_spare.is_some() {
                bail!(
                    "{table}.min_spare/{table}.max_spare are only valid with {table}.scaling = \"dynamic\""
                );
            }
            if other == ScalingKey::Static {
                Scaling::Static
            } else {
                Scaling::Ondemand
            }
        }
    };

    Ok(PoolSettings {
        entrypoint,
        processes,
        mode,
        scaling,
        max_requests: section.max_requests.unwrap_or(0),
        process_idle_timeout: capped_timeout(
            table,
            "process_idle_timeout_secs",
            section.process_idle_timeout_secs.unwrap_or(10),
        )?,
        request_terminate_timeout: capped_timeout(
            table,
            "request_terminate_timeout_secs",
            section.request_terminate_timeout_secs.unwrap_or(0),
        )?,
    })
}

fn resolve_supervisor(
    section: SupervisorSection,
    config_dir: Option<&Path>,
) -> anyhow::Result<SupervisorSettings> {
    // An empty pidfile stays "unset" instead of resolving to the config directory.
    let pidfile = section
        .pidfile
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|p| config_relative(config_dir, p))
        .transpose()?;

    // Zero is rejected here rather than in capped_timeout.
    let control_secs = section.process_control_timeout_secs.unwrap_or(30);
    if control_secs == 0 {
        bail!("supervisor.process_control_timeout_secs must be at least 1");
    }

    Ok(SupervisorSettings {
        process_control_timeout: capped_timeout(
            "supervisor",
            "process_control_timeout_secs",
            control_secs,
        )?,
        pidfile,
    })
}

fn resolve_log(section: LogSection) -> anyhow::Result<LogSettings> {
    // Target names cannot be checked against a known set — they are open-ended
    // module paths — so validation pins the shape EnvFilter parses as a plain
    // target. Anything outside it is filter grammar (`[` opens a span clause,
    // `,`/`=` split directives, a leading symbol is a parse error) and would be
    // reinterpreted or dropped instead of matched.
    for name in section.targets.keys() {
        let mut chars = name.chars();
        let ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-'));
        if !ok {
            bail!(
                "log.targets key `{}` is not a log target: use letters, digits and `_` `:` `.` `-`, starting with a letter, digit or `_`",
                name.escape_default()
            );
        }
    }

    Ok(LogSettings {
        level: section.level.unwrap_or_default(),
        format: section.format.unwrap_or_default(),
        targets: section.targets,
    })
}

/// Largest configurable timeout: caps every `*_secs` key so the master's
/// deadline arithmetic can't overflow.
const MAX_TIMEOUT_SECS: u64 = 86_400;

fn capped_timeout(table: &str, key: &str, secs: u64) -> anyhow::Result<Duration> {
    if secs > MAX_TIMEOUT_SECS {
        bail!("{table}.{key} {secs} is too large (max {MAX_TIMEOUT_SECS})");
    }
    Ok(Duration::from_secs(secs))
}

/// Relative config values hang off the config file's directory, so a config
/// tree is relocatable.
fn config_relative(config_dir: Option<&Path>, value: &str) -> std::io::Result<PathBuf> {
    std::path::absolute(config_dir.unwrap_or_else(|| Path::new(".")).join(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_grace_leaves_room_before_the_master_escalates() {
        let grace = |secs| {
            SupervisorSettings {
                process_control_timeout: Duration::from_secs(secs),
                pidfile: None,
            }
            .drain_grace()
        };
        // The default keeps the 25s-under-30s relationship the front used to hardcode.
        assert_eq!(grace(30), Duration::from_secs(25));
        assert_eq!(grace(60), Duration::from_secs(55));
        assert_eq!(grace(5), Duration::from_millis(2500));
        assert_eq!(grace(1), Duration::from_millis(500));
        // The invariant that matters, across every value the config accepts.
        for secs in 1..=120 {
            assert!(
                grace(secs) < Duration::from_secs(secs),
                "drain must end before the escalation at {secs}s"
            );
        }
    }

    #[test]
    fn listen_parses_all_forms() {
        assert_eq!(
            "127.0.0.1:8000".parse::<Listen>().unwrap(),
            Listen::Tcp(SocketAddr::from(([127, 0, 0, 1], 8000)))
        );
        assert_eq!(
            ":8080".parse::<Listen>().unwrap(),
            Listen::Tcp(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 8080)))
        );
        assert!(matches!("[::1]:8000".parse::<Listen>(), Ok(Listen::Tcp(_))));
        let l: Listen = "unix:/run/rapira.sock".parse().unwrap();
        assert_eq!(l, Listen::Unix(PathBuf::from("/run/rapira.sock")));
        assert_eq!(l.to_string(), "unix:/run/rapira.sock");
    }

    #[test]
    fn listen_rejects_invalid() {
        for bad in ["8080", "", ":", "unix:", "localhost:8000"] {
            assert!(bad.parse::<Listen>().is_err(), "`{bad}` should not parse");
        }
    }

    #[test]
    fn precedence_cli_over_file_over_default() {
        let file = load_str(
            r#"
            [http]
            listen = "0.0.0.0:9000"
            [pool]
            processes = 2
            entrypoint = "app.php"
        "#,
        )
        .unwrap();
        let cli = Overrides {
            listen: Some("127.0.0.1:1234".parse().unwrap()),
            processes: Some(7),
            mode: None,
            entrypoint: Some(PathBuf::from("cli.php")),
        };
        let s = merge(file, cli, Some(Path::new("/etc/rapira"))).unwrap();
        assert_eq!(s.http.listen.to_string(), "127.0.0.1:1234");
        assert_eq!(s.pool.processes, 7);
        assert!(s.pool.entrypoint.is_absolute());
        assert!(s.pool.entrypoint.ends_with("cli.php"));
    }

    #[test]
    fn server_port_derives_from_listen_and_mb_converts() {
        let file = load_str(
            "[http]\nlisten = \":9000\"\nmax_body_size_mb = 2\n[pool]\nentrypoint = \"a.php\"\n",
        )
        .unwrap();
        let s = merge(file, Overrides::default(), Some(Path::new("/w"))).unwrap();
        assert_eq!(s.http.server_port, 9000);
        assert_eq!(s.http.max_body_size, 2 * 1024 * 1024);

        let file =
            load_str("[http]\nlisten = \"unix:/run/r.sock\"\n[pool]\nentrypoint = \"a.php\"\n")
                .unwrap();
        let s = merge(file, Overrides::default(), Some(Path::new("/w"))).unwrap();
        assert_eq!(s.http.server_port, 80);
    }

    #[test]
    fn unsafe_field_names_parses_and_defaults_to_drop() {
        for (text, want) in [
            ("drop", UnsafeFieldNames::Drop),
            ("reject", UnsafeFieldNames::Reject),
        ] {
            let file = load_str(&format!(
                "[http]\nunsafe_field_names = \"{text}\"\n[pool]\nentrypoint = \"a.php\"\n"
            ))
            .unwrap();
            let s = merge(file, Overrides::default(), Some(Path::new("/w"))).unwrap();
            assert_eq!(s.http.unsafe_field_names, want, "{text}");
        }

        let file = load_str("[pool]\nentrypoint = \"a.php\"\n").unwrap();
        let s = merge(file, Overrides::default(), Some(Path::new("/w"))).unwrap();
        assert_eq!(s.http.unsafe_field_names, UnsafeFieldNames::Drop);
    }

    /// A security knob that survives a typo is worse than one that refuses to boot. `allow`
    /// is rejected too: there is no off-switch, so asking for one must fail loudly rather
    /// than quietly leaving the screen on.
    #[test]
    fn unknown_unsafe_field_names_value_is_rejected() {
        for value in ["dorp", "allow"] {
            assert!(
                load_str(&format!(
                    "[http]\nunsafe_field_names = \"{value}\"\n[pool]\nentrypoint = \"a.php\"\n"
                ))
                .is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn file_entrypoint_is_config_dir_relative() {
        let file = load_str("[pool]\nentrypoint = \"public/index.php\"\n").unwrap();
        let s = merge(file, Overrides::default(), Some(Path::new("/srv/app"))).unwrap();
        // Compare through the same absolute() rule: Windows prefixes the current
        // drive (D:\srv\...), so the bare literal would not match there.
        assert_eq!(
            s.pool.entrypoint,
            std::path::absolute("/srv/app/public/index.php").unwrap()
        );
    }

    #[test]
    fn entrypoint_is_required() {
        let err = merge(FileConfig::default(), Overrides::default(), None).unwrap_err();
        assert!(err.to_string().contains("entrypoint"));

        // An empty string must hit the same clear error, not resolve to the config dir.
        let file = load_str("[pool]\nentrypoint = \"\"\n").unwrap();
        let err = merge(file, Overrides::default(), Some(Path::new("/srv/app"))).unwrap_err();
        assert!(err.to_string().contains("no entrypoint"));
    }

    #[test]
    fn max_body_size_overflow_is_rejected() {
        // 2^44 MB would wrap the byte conversion to 0 on a 64-bit usize.
        let file =
            load_str("[http]\nmax_body_size_mb = 17592186044416\n[pool]\nentrypoint = \"a.php\"\n")
                .unwrap();
        let err = merge(file, Overrides::default(), Some(Path::new("/w"))).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(load_str("[pool]\nbogus = 1\n").is_err());
        assert!(load_str("[nope]\nx = 1\n").is_err());
        assert!(load_str("[supervisor]\nbogus = 1\n").is_err());
        // removed keys and tables are errors, not aliases
        assert!(load_str("[pool]\nthreads = 1\n").is_err());
        assert!(load_str("[pool]\nclassic = true\n").is_err());
        assert!(load_str("[pm]\nmode = \"static\"\n").is_err());
        // every key has exactly one home
        assert!(load_str("[pool]\npidfile = \"r.pid\"\n").is_err());
        assert!(load_str("[supervisor]\nmax_requests = 1\n").is_err());
        // [log] denies unknown keys and unknown enum values alike.
        assert!(load_str("[log]\nbogus = 1\n").is_err());
        assert!(load_str("[log]\nlevel = \"verbose\"\n").is_err());
        assert!(load_str("[log]\nformat = \"pretty\"\n").is_err());
    }

    #[test]
    fn timeout_caps_name_the_key_that_broke() {
        for (toml, key) in [
            (
                "[pool]\nentrypoint = \"a.php\"\nprocess_idle_timeout_secs = 100000\n",
                "pool.process_idle_timeout_secs",
            ),
            (
                "[pool]\nentrypoint = \"a.php\"\nrequest_terminate_timeout_secs = 100000\n",
                "pool.request_terminate_timeout_secs",
            ),
            (
                "[pool]\nentrypoint = \"a.php\"\n[supervisor]\nprocess_control_timeout_secs = 100000\n",
                "supervisor.process_control_timeout_secs",
            ),
            (
                "[http]\nwrite_timeout_secs = 100000\n[pool]\nentrypoint = \"a.php\"\n",
                "http.write_timeout_secs",
            ),
        ] {
            let err = merge(
                load_str(toml).unwrap(),
                Overrides::default(),
                Some(Path::new("/w")),
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains(key) && err.contains("too large"),
                "{key}: {err}"
            );
        }

        // 86400 is the cap, not the first rejected value.
        let file = load_str("[pool]\nentrypoint = \"a.php\"\nprocess_idle_timeout_secs = 86400\n")
            .unwrap();
        assert!(merge(file, Overrides::default(), Some(Path::new("/w"))).is_ok());
    }

    /// A zero stop budget escalates the instant it starts, leaving the drain no
    /// time, so it is rejected even though zero means "off" for its siblings.
    #[test]
    fn supervisor_control_timeout_zero_is_rejected() {
        let file = load_str(
            "[pool]\nentrypoint = \"a.php\"\n[supervisor]\nprocess_control_timeout_secs = 0\n",
        )
        .unwrap();
        let err = merge(file, Overrides::default(), Some(Path::new("/w")))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("supervisor.process_control_timeout_secs must be at least 1"),
            "{err}"
        );
    }

    /// The floor is checked on the resolved value, after precedence, so a zero from
    /// either layer must fail — including a CLI `0` shadowing a usable file value.
    #[test]
    fn pool_processes_zero_is_rejected_from_either_layer() {
        let file = load_str("[pool]\nprocesses = 0\nentrypoint = \"a.php\"\n").unwrap();
        let err = merge(file, Overrides::default(), Some(Path::new("/w")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("pool.processes must be at least 1"), "{err}");

        let file = load_str("[pool]\nprocesses = 4\nentrypoint = \"a.php\"\n").unwrap();
        let err = merge(
            file,
            Overrides {
                processes: Some(0),
                ..Default::default()
            },
            Some(Path::new("/w")),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("pool.processes must be at least 1"), "{err}");
    }

    /// `[http.uploads]` resolves every knob (with unit conversion, dir relative
    /// to the config) and rejects a zero max_files like its siblings — zero
    /// would 413 every file part while booting clean.
    #[test]
    fn http_uploads_resolve_and_reject_zero_files() {
        let file = load_str(
            "[pool]\nentrypoint = \"a.php\"\n[http.uploads]\ndir = \"spool\"\n\
             max_file_size_mb = 3\nmax_field_size_kb = 7\nmax_files = 4\n\
             max_parts = 9\nmax_part_headers = 5\n",
        )
        .unwrap();
        let s = merge(file, Overrides::default(), Some(Path::new("/w"))).unwrap();
        let u = &s.http.uploads;
        assert_eq!(u.dir, Path::new("/w/spool"));
        assert_eq!(u.max_file_size, 3 * 1024 * 1024);
        assert_eq!(u.max_field_size, 7 * 1024);
        assert_eq!(u.max_files, 4);
        assert_eq!(u.max_parts, 9);
        assert_eq!(u.max_part_headers, 5);

        let file =
            load_str("[pool]\nentrypoint = \"a.php\"\n[http.uploads]\nmax_files = 0\n").unwrap();
        let err = merge(file, Overrides::default(), Some(Path::new("/w")))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("http.uploads.max_files must be at least 1"),
            "{err}"
        );
    }

    /// `ondemand` and `static` share one match arm: the arm still has to tell them
    /// apart, and both reject the dynamic-only spare keys.
    #[test]
    fn pool_ondemand_and_static_scaling_resolve() {
        for (key, want) in [("ondemand", Scaling::Ondemand), ("static", Scaling::Static)] {
            let file = load_str(&format!(
                "[pool]\nentrypoint = \"a.php\"\nscaling = \"{key}\"\n"
            ))
            .unwrap();
            let s = merge(file, Overrides::default(), Some(Path::new("/w"))).unwrap();
            assert_eq!(s.pool.scaling, want, "{key}");
        }

        let file =
            load_str("[pool]\nentrypoint = \"a.php\"\nscaling = \"ondemand\"\nmax_spare = 2\n")
                .unwrap();
        let err = merge(file, Overrides::default(), Some(Path::new("/w")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("only valid with pool.scaling"), "{err}");
    }

    /// `pool.mode` picks the run mode; the CLI override wins over the file in
    /// both directions (`--mode dispatcher` can undo a file's `classic`).
    #[test]
    fn pool_run_mode_resolves_with_cli_precedence() {
        for (key, want) in [
            ("classic", RunMode::Classic),
            ("worker", RunMode::Worker),
            ("dispatcher", RunMode::Dispatcher),
        ] {
            let file = load_str(&format!(
                "[pool]\nentrypoint = \"a.php\"\nmode = \"{key}\"\n"
            ))
            .unwrap();
            let s = merge(file, Overrides::default(), Some(Path::new("/w"))).unwrap();
            assert_eq!(s.pool.mode, want, "{key}");
        }

        let file = load_str("[pool]\nentrypoint = \"a.php\"\n").unwrap();
        let s = merge(file, Overrides::default(), Some(Path::new("/w"))).unwrap();
        assert_eq!(s.pool.mode, RunMode::Dispatcher, "default");

        let file = load_str("[pool]\nentrypoint = \"a.php\"\nmode = \"classic\"\n").unwrap();
        let s = merge(
            file,
            Overrides {
                mode: Some(RunMode::Dispatcher),
                ..Default::default()
            },
            Some(Path::new("/w")),
        )
        .unwrap();
        assert_eq!(s.pool.mode, RunMode::Dispatcher, "CLI beats file");

        assert!(load_str("[pool]\nentrypoint = \"a.php\"\nmode = \"async\"\n").is_err());
    }

    /// `[http.uploads]` configures the host-side multipart parser, which only
    /// dispatcher mode runs: an explicit table under any other mode is a boot
    /// error, absence stays silent.
    #[test]
    fn http_uploads_require_dispatcher_mode() {
        for mode in ["classic", "worker"] {
            let file = load_str(&format!(
                "[pool]\nentrypoint = \"a.php\"\nmode = \"{mode}\"\n[http.uploads]\nmax_files = 4\n"
            ))
            .unwrap();
            let err = merge(file, Overrides::default(), Some(Path::new("/w")))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("dispatcher mode only") && err.contains(mode),
                "{err}"
            );
        }

        // Even an empty table is presence, and the dispatcher default accepts it.
        let file = load_str("[pool]\nentrypoint = \"a.php\"\n[http.uploads]\n").unwrap();
        assert!(merge(file, Overrides::default(), Some(Path::new("/w"))).is_ok());

        // Absence resolves to defaults under every mode.
        let file = load_str("[pool]\nentrypoint = \"a.php\"\nmode = \"classic\"\n").unwrap();
        assert!(merge(file, Overrides::default(), Some(Path::new("/w"))).is_ok());
    }

    #[test]
    fn pool_dynamic_requires_valid_spares() {
        let merged = |keys: &str, cli: Overrides| {
            let file = load_str(&format!(
                "[pool]\nprocesses = 4\nentrypoint = \"a.php\"\n{keys}"
            ))
            .unwrap();
            merge(file, cli, Some(Path::new("/w")))
        };

        // spares required
        assert!(merged("scaling = \"dynamic\"\n", Overrides::default()).is_err());
        assert!(
            merged(
                "scaling = \"dynamic\"\nmin_spare = 3\nmax_spare = 2\n",
                Overrides::default()
            )
            .is_err()
        );

        let err = merged(
            "scaling = \"dynamic\"\nmin_spare = 1\nmax_spare = 5\n",
            Overrides::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("pool spares") && err.contains("pool.processes (4)"),
            "{err}"
        );

        let err = merged(
            "scaling = \"static\"\nmin_spare = 1\nmax_spare = 2\n",
            Overrides::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("only valid with pool.scaling"), "{err}");

        // --processes lowers the ceiling the spares are validated against.
        let err = merged(
            "scaling = \"dynamic\"\nmin_spare = 1\nmax_spare = 3\n",
            Overrides {
                processes: Some(2),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("pool.processes (2)"), "{err}");

        let s = merged(
            "scaling = \"dynamic\"\nmin_spare = 1\nmax_spare = 3\nmax_requests = 500\n",
            Overrides::default(),
        )
        .unwrap();
        assert_eq!(
            s.pool.scaling,
            Scaling::Dynamic {
                min_spare: 1,
                max_spare: 3
            }
        );
        assert_eq!(s.pool.max_requests, 500);
    }

    #[test]
    fn supervisor_pidfile_resolves_against_config_dir() {
        let file =
            load_str("[pool]\nentrypoint = \"a.php\"\n[supervisor]\npidfile = \"rapira.pid\"\n")
                .unwrap();
        let s = merge(file, Overrides::default(), Some(Path::new("/etc/rapira"))).unwrap();
        assert_eq!(
            s.supervisor.pidfile.as_deref(),
            Some(Path::new("/etc/rapira/rapira.pid"))
        );
    }

    /// The filter string is assembled from these keys, so a key carrying filter
    /// syntax would inject directives (`"php=trace,tokio" = "debug"` reads as two).
    #[test]
    fn log_target_names_that_would_corrupt_the_filter_are_rejected() {
        for entry in [
            "\"\" = \"info\"",
            "\"php=trace,tokio\" = \"info\"",
            "\"a b\" = \"info\"",
            "\"a/b\" = \"info\"",
            "\"a\\u001Bb\" = \"info\"",
            // EnvFilter grammar, not target text: a span clause and a leading symbol.
            "\"http[request]\" = \"info\"",
            "\".php\" = \"info\"",
        ] {
            let file = load_str(&format!(
                "[pool]\nentrypoint = \"a.php\"\n[log.targets]\n{entry}\n"
            ))
            .unwrap();
            assert!(
                merge(file, Overrides::default(), Some(Path::new("/w"))).is_err(),
                "{entry}"
            );
        }
    }
}
