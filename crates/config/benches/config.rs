//! Benchmarks for rapira's configuration resolution.
//!
//! Two hot paths are covered through the crate's public API:
//! - [`Listen`] parsing, exercised on every listen-address input form.
//! - [`resolve`], which reads a `rapira.toml`, parses it, layers CLI overrides,
//!   and validates the result into a `Settings`.

use std::io::Write;
use std::path::PathBuf;

use divan::{Bencher, black_box};
use rapira_config::{Listen, Overrides, resolve};

fn main() {
    divan::main();
}

/// Parse each supported (and one rejected) listen-address form.
#[divan::bench(args = [
    "127.0.0.1:8000",
    "0.0.0.0:9000",
    ":8080",
    "[::1]:8000",
    "unix:/run/rapira.sock",
    "not-an-address",
])]
fn parse_listen(input: &str) -> Result<Listen, impl std::error::Error> {
    black_box(input).parse::<Listen>()
}

/// A representative `rapira.toml` covering every section and key.
const FULL_CONFIG: &str = r#"
[http]
listen = "0.0.0.0:9000"
server_name = "example.com"
server_port = 8080
max_body_size_mb = 16

[pool]
threads = 4
entrypoint = "public/index.php"
classic = true
"#;

/// A minimal `rapira.toml` that leans on defaults for most keys.
const MINIMAL_CONFIG: &str = r#"
[pool]
entrypoint = "index.php"
"#;

/// Resolve settings from a full on-disk config file (parse + merge + validate).
#[divan::bench]
fn resolve_from_file_full(bencher: Bencher) {
    resolve_from_file(bencher, FULL_CONFIG);
}

/// Resolve settings from a minimal on-disk config file (leans on defaults).
#[divan::bench]
fn resolve_from_file_minimal(bencher: Bencher) {
    resolve_from_file(bencher, MINIMAL_CONFIG);
}

/// Write `config` to a temp file and measure only the `resolve` call.
fn resolve_from_file(bencher: Bencher, config: &str) {
    let mut file = tempfile();
    file.file.write_all(config.as_bytes()).unwrap();
    file.file.flush().unwrap();
    let path = file.path.clone();

    bencher.bench(|| resolve(Some(black_box(&path)), Overrides::default()));
}

/// Resolve settings driven purely by CLI overrides (no config file on disk).
#[divan::bench]
fn resolve_from_cli(bencher: Bencher) {
    bencher
        .with_inputs(|| Overrides {
            listen: Some("127.0.0.1:1234".parse().unwrap()),
            threads: Some(8),
            classic: true,
            entrypoint: Some(PathBuf::from("worker.php")),
        })
        .bench_values(|cli| resolve(None, black_box(cli)));
}

/// A self-cleaning temp file that lives on a real filesystem path.
struct TempFile {
    file: std::fs::File,
    path: PathBuf,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn tempfile() -> TempFile {
    let mut path = std::env::temp_dir();
    let unique = format!(
        "rapira-bench-{}-{}.toml",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    path.push(unique);
    let file = std::fs::File::create(&path).unwrap();
    TempFile { file, path }
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
