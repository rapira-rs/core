//! Resolves rapira's runtime settings from three layers, in order of precedence:
//! CLI flags > `rapira.toml` > built-in defaults. Everything collapses into one
//! validated [`Settings`], the single struct `main` consumes. (Env vars are a
//! later layer and are intentionally absent here.)

use anyhow::{Context, bail};
use serde::Deserialize;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;

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
    pub threads: Option<usize>,
    /// `--classic`; force-on only (there is no `--no-classic`).
    pub classic: bool,
    /// Positional `SCRIPT`; overrides `pool.entrypoint`.
    pub entrypoint: Option<PathBuf>,
}

/// The one validated settings struct the server boots from.
#[derive(Debug)]
pub struct Settings {
    pub http: HttpSettings,
    pub pool: PoolSettings,
}

#[derive(Debug)]
pub struct HttpSettings {
    pub listen: Listen,
    pub server_name: String,
    /// What PHP sees as SERVER_PORT; defaults to the listen TCP port (80 for unix:).
    pub server_port: u16,
    /// Bytes, converted from the config's `max_body_size_mb`.
    pub max_body_size: usize,
}

#[derive(Debug)]
pub struct PoolSettings {
    pub threads: usize,
    /// Absolute path to the PHP entry script.
    pub entrypoint: PathBuf,
    pub classic: bool,
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
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpSection {
    listen: Option<String>,
    server_name: Option<String>,
    server_port: Option<u16>,
    max_body_size_mb: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PoolSection {
    threads: Option<usize>,
    entrypoint: Option<String>,
    classic: Option<bool>,
}

/// Default worker count: one per logical CPU. Falls back to 1 if the platform can't
/// report it. (On NTS this is clamped back to 1 inside `Rapira::start`.)
fn default_threads() -> usize {
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
    let listen = match cli.listen {
        Some(l) => l,
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

    let threads = cli.threads.or(file.pool.threads).unwrap_or_else(default_threads);
    if threads == 0 {
        bail!("threads must be at least 1");
    }

    let classic = cli.classic || file.pool.classic.unwrap_or(false);

    // Positional SCRIPT is cwd-relative; a config `pool.entrypoint` is resolved against
    // the config file's directory so the config is relocatable.
    // `.filter` routes an empty `entrypoint = ""` to the clear bail below instead of
    // letting `base.join("")` silently resolve to the config directory.
    let entrypoint = if let Some(script) = cli.entrypoint {
        std::path::absolute(&script)?
    } else if let Some(ep) = file.pool.entrypoint.as_deref().filter(|s| !s.is_empty()) {
        let base = config_dir.unwrap_or_else(|| Path::new("."));
        std::path::absolute(base.join(ep))?
    } else {
        bail!("no entrypoint: pass a SCRIPT argument or set pool.entrypoint in the config file");
    };

    Ok(Settings {
        http: HttpSettings {
            listen,
            server_name: file.http.server_name.unwrap_or_else(|| "localhost".to_owned()),
            server_port,
            max_body_size,
        },
        pool: PoolSettings {
            threads,
            entrypoint,
            classic,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
            threads = 2
            entrypoint = "app.php"
        "#,
        )
        .unwrap();
        let cli = Overrides {
            listen: Some("127.0.0.1:1234".parse().unwrap()),
            threads: Some(7),
            classic: false,
            entrypoint: Some(PathBuf::from("cli.php")),
        };
        let s = merge(file, cli, Some(Path::new("/etc/rapira"))).unwrap();
        assert_eq!(s.http.listen.to_string(), "127.0.0.1:1234");
        assert_eq!(s.pool.threads, 7);
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

        let file = load_str(
            "[http]\nlisten = \"unix:/run/r.sock\"\n[pool]\nentrypoint = \"a.php\"\n",
        )
        .unwrap();
        let s = merge(file, Overrides::default(), Some(Path::new("/w"))).unwrap();
        assert_eq!(s.http.server_port, 80);
    }

    #[test]
    fn file_entrypoint_is_config_dir_relative() {
        let file = load_str("[pool]\nentrypoint = \"public/index.php\"\n").unwrap();
        let s = merge(file, Overrides::default(), Some(Path::new("/srv/app"))).unwrap();
        assert_eq!(s.pool.entrypoint, PathBuf::from("/srv/app/public/index.php"));
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
        let file = load_str(
            "[http]\nmax_body_size_mb = 17592186044416\n[pool]\nentrypoint = \"a.php\"\n",
        )
        .unwrap();
        let err = merge(file, Overrides::default(), Some(Path::new("/w"))).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(load_str("[pool]\nbogus = 1\n").is_err());
        assert!(load_str("[nope]\nx = 1\n").is_err());
    }
}
