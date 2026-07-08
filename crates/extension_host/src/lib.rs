//! Loads WASM extensions and runs them as PHP-request drivers.
//!
//! An extension is a sidecar component: `ExtensionHost::load` compiles and
//! validates each one, and `run` spawns a driver thread per extension that calls
//! `init` then `handle`. The guest drives PHP through the `exec` host imports,
//! which submit requests to rapira's worker pool via a [`RapiraHandle`].

mod host_fns;
mod instances;
pub mod manifest;
mod state;
mod wit;

use anyhow::{Context, bail};
use manifest::Manifest;
use php_sys::RapiraHandle;
use state::HostState;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine};
use wit::{Extension, ExtensionPre};

/// The guest's `rapira:api-version` stamp must match this major/minor.
const SUPPORTED_MAJOR: u16 = 0;
const SUPPORTED_MINOR: u16 = 1;

/// Worker threads for the shared extension runtime. Extension tasks are
/// PHP-I/O-bound (they mostly await `exec`), so a small pool multiplexes many.
const EXT_WORKER_THREADS: usize = 2;

/// major.minor.patch, from the guest's `rapira:api-version` custom section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ApiVersion {
    major: u16,
    minor: u16,
    patch: u16,
}

/// `wasmtime::Error` is a distinct type from `anyhow::Error`, but converts into one.
fn wt(e: wasmtime::Error) -> anyhow::Error {
    e.into()
}

struct LoadedExt {
    id: Arc<str>,
    /// Import satisfiability is proven at load, not at first run.
    pre: ExtensionPre<HostState>,
}

pub struct ExtensionHost {
    engine: Engine,
    exts: Vec<LoadedExt>,
}

impl ExtensionHost {
    /// Scan `dir` and prepare every extension it contains. Synchronous; no guest
    /// code runs.
    pub fn load(dir: &Path) -> anyhow::Result<Arc<Self>> {
        let engine = build_engine()?;
        let linker = build_linker(&engine)?;

        let mut exts: Vec<LoadedExt> = Vec::new();
        for package in discover(dir)? {
            let manifest = Manifest::load(&package.join("extension.toml"))?;
            if exts.iter().any(|e| *e.id == manifest.id) {
                bail!("duplicate extension id {:?} in {}", manifest.id, dir.display());
            }

            let bytes = std::fs::read(package.join("extension.wasm"))
                .with_context(|| format!("reading {}/extension.wasm", package.display()))?;
            check_api_version(&manifest.id, &bytes)?;

            let component = Component::from_binary(&engine, &bytes)
                .map_err(wt)
                .with_context(|| format!("extension {}: invalid component", manifest.id))?;
            // Type-checks the guest's imports against the host linker now (no guest
            // code, no store), so SDK/WIT skew is a load error, not a run failure.
            let pre = ExtensionPre::new(
                linker
                    .instantiate_pre(&component)
                    .map_err(wt)
                    .with_context(|| format!("extension {}: unsatisfied imports", manifest.id))?,
            )
            .map_err(wt)
            .with_context(|| format!("extension {}: world mismatch", manifest.id))?;

            exts.push(LoadedExt {
                id: manifest.id.into(),
                pre,
            });
        }

        log::info!(
            "[rapira] loaded {} extension(s) from {}",
            exts.len(),
            dir.display()
        );
        Ok(Arc::new(Self { engine, exts }))
    }

    /// Run every extension as a task on a shared runtime (each calls `run`). The
    /// returned guard awaits them on drop, which drops their `RapiraHandle`s — do
    /// this **before** dropping `Rapira`, or its own `Drop` hangs (shutdown
    /// contract).
    pub fn run(self: &Arc<Self>, rapira: RapiraHandle) -> Running {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(EXT_WORKER_THREADS)
            .thread_name("rapira-ext")
            .build()
            .expect("build extension runtime");

        let handles = self
            .exts
            .iter()
            .map(|ext| {
                let (engine, pre, id) = (self.engine.clone(), ext.pre.clone(), ext.id.clone());
                let rapira = rapira.clone();
                rt.spawn(async move { instances::drive(engine, id, pre, rapira).await })
            })
            .collect();

        Running { rt, handles }
    }
}

/// Awaits the extension tasks on drop; `join` surfaces their outcomes first.
pub struct Running {
    rt: Runtime,
    handles: Vec<JoinHandle<Result<(), String>>>,
}

impl Running {
    /// Wait for every extension and return its outcome (`Ok` = `run` returned
    /// `Ok`). Consumes the guard.
    pub fn join(mut self) -> Vec<Result<(), String>> {
        let handles = std::mem::take(&mut self.handles);
        self.rt.block_on(async {
            let mut out = Vec::with_capacity(handles.len());
            for h in handles {
                out.push(h.await.unwrap_or_else(|_| Err("driver task panicked".into())));
            }
            out
        })
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let handles = std::mem::take(&mut self.handles);
        if !handles.is_empty() {
            self.rt.block_on(async {
                for h in handles {
                    let _ = h.await;
                }
            });
        }
    }
}

fn build_engine() -> anyhow::Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    Engine::new(&config)
        .map_err(wt)
        .context("building the wasm engine")
}

fn build_linker(engine: &Engine) -> anyhow::Result<Linker<HostState>> {
    let mut linker = Linker::new(engine);
    // Mandatory even for a no-capability extension: a wasm32-wasip2 std guest
    // imports wasi:cli and clocks.
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(wt)
        .context("adding wasi to the linker")?;
    Extension::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s)
        .map_err(wt)
        .context("adding rapira host functions to the linker")?;
    Ok(linker)
}

/// One level deep, lexicographic, dot-prefixed dirs ignored (staging). A package
/// missing either file is a hard error, never a skip.
fn discover(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !dir.exists() {
        log::info!("[rapira] no extension directory at {}", dir.display());
        return Ok(Vec::new());
    }

    let mut packages = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if !entry.file_type()?.is_dir() || name.starts_with('.') {
            continue;
        }

        let (toml, wasm) = (path.join("extension.toml"), path.join("extension.wasm"));
        match (toml.exists(), wasm.exists()) {
            (true, true) => packages.push(path),
            (true, false) => bail!("{}: extension.toml without extension.wasm", path.display()),
            (false, true) => bail!("{}: extension.wasm without extension.toml", path.display()),
            (false, false) => continue,
        }
    }

    packages.sort();
    Ok(packages)
}

fn check_api_version(id: &str, wasm: &[u8]) -> anyhow::Result<()> {
    let v = read_api_version(wasm)
        .with_context(|| format!("extension {id}: reading rapira:api-version"))?
        .with_context(|| {
            format!("extension {id} has no rapira:api-version section; rebuild it against the SDK")
        })?;

    if v.major != SUPPORTED_MAJOR || v.minor != SUPPORTED_MINOR {
        bail!(
            "extension {id} targets rapira api {}.{}.{}, but this host supports \
             {SUPPORTED_MAJOR}.{SUPPORTED_MINOR}.x",
            v.major,
            v.minor,
            v.patch
        );
    }
    Ok(())
}

/// The stamp lives in the core module nested inside the component, so the whole
/// binary is walked. Parsing it all first turns malformed input into an `Err`
/// before wasmtime can panic on it.
fn read_api_version(wasm: &[u8]) -> anyhow::Result<Option<ApiVersion>> {
    let mut version = None;
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let wasmparser::Payload::CustomSection(section) = payload?
            && section.name() == "rapira:api-version"
        {
            let data = section.data();
            let bytes: [u8; 6] = data.try_into().map_err(|_| {
                anyhow::anyhow!("rapira:api-version must be 6 bytes, got {}", data.len())
            })?;
            version = Some(ApiVersion {
                major: u16::from_be_bytes([bytes[0], bytes[1]]),
                minor: u16::from_be_bytes([bytes[2], bytes[3]]),
                patch: u16::from_be_bytes([bytes[4], bytes[5]]),
            });
        }
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_stamp_round_trips() {
        let mut wasm = b"\0asm\x0d\0\x01\0".to_vec();
        let name = b"rapira:api-version";
        let data = [0u8, 0, 0, 1, 0, 3];
        let mut body = vec![name.len() as u8];
        body.extend_from_slice(name);
        body.extend_from_slice(&data);
        wasm.push(0);
        wasm.push(body.len() as u8);
        wasm.extend_from_slice(&body);

        assert_eq!(
            read_api_version(&wasm).expect("parse"),
            Some(ApiVersion {
                major: 0,
                minor: 1,
                patch: 3
            })
        );
    }

    #[test]
    fn missing_directory_is_not_an_error() {
        let packages = discover(Path::new("/nonexistent/rapira/extensions")).expect("discover");
        assert!(packages.is_empty());
    }

    /// The host is shared across driver threads.
    #[test]
    fn extension_host_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ExtensionHost>();
    }
}
