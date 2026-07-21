//! CodSpeed benchmarks for `rapira_config`.
//!
//! Covers the crate's public surface: parsing listen addresses in every accepted
//! form, and the full `resolve` path (read file -> TOML parse -> merge -> validate).

use std::io::Write;
use std::path::PathBuf;

use rapira_config::{Listen, Overrides, resolve};

fn main() {
    divan::main();
}

/// Parse each accepted listen-address form. These strings flow through clap's
/// derived value parser on every startup and via `http.listen` in the config file.
#[divan::bench(args = [
    "127.0.0.1:8000",
    ":8080",
    "[::1]:8000",
    "unix:/run/rapira.sock",
])]
fn parse_listen(bencher: divan::Bencher, input: &str) {
    bencher.bench(|| divan::black_box(input).parse::<Listen>());
}

/// Write a representative `rapira.toml` to a temp file once, then benchmark the
/// full `resolve` path against it. The file write is done in `with_inputs` so only
/// the read + TOML parse + merge + validate is measured.
#[divan::bench]
fn resolve_from_file(bencher: divan::Bencher) {
    let toml = r#"
        [http]
        listen = "0.0.0.0:9000"
        server_name = "example.com"
        server_port = 8443
        max_body_size_mb = 16

        [pool]
        threads = 8
        entrypoint = "public/index.php"
        classic = true
    "#;

    bencher
        .with_inputs(|| {
            let mut file = tempfile();
            file.write_all(toml.as_bytes()).unwrap();
            file.flush().unwrap();
            file
        })
        .bench_values(|file| {
            let path = file.path().to_owned();
            divan::black_box(resolve(Some(&path), Overrides::default()))
        });
}

/// Resolve with no config file: pure defaults + a CLI entrypoint override. This is
/// the turnkey-flags path (no TOML parsing), exercising `merge`/validation alone.
#[divan::bench]
fn resolve_defaults(bencher: divan::Bencher) {
    bencher
        .with_inputs(|| Overrides {
            entrypoint: Some(PathBuf::from("worker.php")),
            ..Overrides::default()
        })
        .bench_values(|overrides| divan::black_box(resolve(None, overrides)));
}

/// A minimal `NamedTempFile` replacement so the crate needs no extra dev-dependency.
struct TempFile {
    path: PathBuf,
    file: std::fs::File,
}

impl TempFile {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Write for TempFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn tempfile() -> TempFile {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "rapira_config_bench_{}_{n}.toml",
        std::process::id()
    ));
    let file = std::fs::File::create(&path).unwrap();
    TempFile { path, file }
}
