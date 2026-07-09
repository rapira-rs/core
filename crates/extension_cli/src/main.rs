//! `rapira-extension` — compile an extension and package it into `dist/`.
//!
//! Compile + package only, like zed's extension CLI. The host validates the
//! api-version and the component itself at load, so there is nothing to check here.

use anyhow::{Context, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, exit};

const TARGET: &str = "wasm32-wasip2";

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (Some("build"), Some(dir)) => build(Path::new(&dir)),
        _ => {
            eprintln!(
                "usage: rapira-extension build <extension-dir>   # writes <extension-dir>/dist/"
            );
            exit(2);
        }
    }
}

fn build(dir: &Path) -> anyhow::Result<()> {
    let manifest = dir.join("extension.toml");
    if !manifest.exists() {
        bail!("{}: no extension.toml", dir.display());
    }

    ensure_target()?;

    // Force the output into <dir>/target so the artifact path is deterministic even
    // when the extension is a workspace member (zed does the same with --target-dir).
    let status: ExitStatus =
        Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .current_dir(dir)
            .args([
                "build",
                "--target",
                TARGET,
                "--release",
                "--target-dir",
                "target",
            ])
            .status()
            .context("running cargo build")?;
    if !status.success() {
        bail!("cargo build failed");
    }

    let wasm: PathBuf = wasm_artifact(dir, &dir.join("target").join(TARGET).join("release"))?;

    let dist: PathBuf = dir.join("dist");
    std::fs::create_dir_all(&dist)?;
    std::fs::copy(&wasm, dist.join("extension.wasm"))?;
    std::fs::copy(&manifest, dist.join("extension.toml"))?;

    println!("packaged {} -> {}", wasm.display(), dist.display());
    Ok(())
}

/// The component cargo produced, resolved by package name: the
/// `wasm32-wasip2` target writes `<release>/<name>.wasm`, normalizing `-` to `_`.
fn wasm_artifact(dir: &Path, release: &Path) -> anyhow::Result<PathBuf> {
    let cargo_toml = dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&cargo_toml)
        .with_context(|| format!("reading {}", cargo_toml.display()))?;
    let name = text
        .parse::<toml::Table>()
        .with_context(|| format!("parsing {}", cargo_toml.display()))?
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(|name| name.as_str())
        .context("Cargo.toml has no [package].name")?
        .replace('-', "_");

    let wasm: PathBuf = release.join(format!("{name}.wasm"));
    if !wasm.exists() {
        bail!(
            "no {}; is the crate `crate-type = [\"cdylib\"]`?",
            wasm.display()
        );
    }
    Ok(wasm)
}

/// Add the wasm target if rustup is present and it is missing (like zed's builder).
fn ensure_target() -> anyhow::Result<()> {
    let Ok(out) = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
    else {
        // No rustup (e.g. a distro toolchain): let cargo report a missing target.
        return Ok(());
    };
    if String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.trim() == TARGET)
    {
        return Ok(());
    }

    println!("installing the {TARGET} target");
    let status = Command::new("rustup")
        .args(["target", "add", TARGET])
        .status()
        .context("running rustup target add")?;
    if !status.success() {
        bail!("rustup target add {TARGET} failed");
    }
    Ok(())
}
