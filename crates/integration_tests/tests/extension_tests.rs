//! End-to-end: a WASM extension drives a request through PHP via the host's
//! `exec` import.
//!
//! Skips when `examples/hello` has not been built for wasm32-wasip2 — the same
//! degrade-instead-of-fail shape the observer suites use.

use extension_host::ExtensionHost;
use integration_tests::{fixture, php_lock};
use php_sys::{Mode, Rapira};
use std::path::{Path, PathBuf};

const HELLO_MANIFEST: &str = "id = \"hello\"\nname = \"Hello\"\nversion = \"0.1.0\"\n";

/// `cargo build -p hello --target wasm32-wasip2 --release` in the extensions repo.
fn hello_component() -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../extensions/target/wasm32-wasip2/release/hello.wasm");
    path.exists().then_some(path)
}

/// Lay out `<ext_dir>/<id>/{extension.wasm, extension.toml}` in a scratch dir.
fn install(id: &str, manifest: &str) -> anyhow::Result<Option<PathBuf>> {
    let Some(wasm) = hello_component() else {
        return Ok(None);
    };
    let root = std::env::temp_dir().join(format!("rapira-ext-{}-{id}", std::process::id()));
    let package = root.join(id);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&package)?;
    // Not std::fs::copy: a libphp built with --enable-zend-test exports its own
    // copy_file_range, which dereferences a ZTS global that does not exist yet.
    std::fs::write(package.join("extension.wasm"), std::fs::read(&wasm)?)?;
    std::fs::write(package.join("extension.toml"), manifest)?;
    Ok(Some(root))
}

#[test]
fn an_extension_drives_concurrent_requests_through_php() -> anyhow::Result<()> {
    let _guard = php_lock();
    let Some(dir) = install("hello", HELLO_MANIFEST)? else {
        return Ok(()); // hello.wasm not built
    };

    // Worker mode: the resident script answers each `exec` request with "ok:<from>".
    // Two workers so the extension's two `join!`ed execs can run in parallel.
    let rapira = Rapira::start(Mode::Worker(fixture("ext-driver-worker.php")), 2)?;
    let ext = ExtensionHost::load(&dir)?;

    // hello's async `run` `join!`s `GET /?from=a` and `GET /?from=b`, then checks
    // each response is 200 with its own distinct body ("ok:a" / "ok:b") — so an Ok
    // outcome proves both concurrent exec subtasks ran and returned their own result.
    let running = ext.run(rapira.handle()?);
    let outcomes = running.join();

    assert_eq!(outcomes.len(), 1);
    assert!(
        outcomes[0].is_ok(),
        "extension driver failed: {:?}",
        outcomes[0]
    );

    drop(rapira);
    Ok(())
}

#[test]
fn many_extensions_run_concurrently() -> anyhow::Result<()> {
    let _guard = php_lock();
    let Some(wasm) = hello_component() else {
        return Ok(());
    };

    // 12 copies of the hello driver, each with a distinct id, in one ext dir.
    const N: usize = 12;
    let root = std::env::temp_dir().join(format!("rapira-ext-many-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bytes = std::fs::read(&wasm)?;
    for i in 0..N {
        let package = root.join(format!("ext{i}"));
        std::fs::create_dir_all(&package)?;
        std::fs::write(package.join("extension.wasm"), &bytes)?;
        std::fs::write(package.join("extension.toml"), format!("id = \"ext{i}\"\n"))?;
    }

    // A few PHP workers so the fan-out has real concurrency to exploit.
    let rapira = Rapira::start(Mode::Worker(fixture("ext-driver-worker.php")), 4)?;
    let ext = ExtensionHost::load(&root)?;

    let outcomes = ext.run(rapira.handle()?).join();
    assert_eq!(outcomes.len(), N);
    assert!(
        outcomes.iter().all(|r| r.is_ok()),
        "some extensions failed: {outcomes:?}"
    );

    drop(rapira);
    Ok(())
}

#[test]
fn a_package_missing_its_wasm_fails_to_load() -> anyhow::Result<()> {
    let root = std::env::temp_dir().join(format!("rapira-ext-torn-{}", std::process::id()));
    let package = root.join("torn");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&package)?;
    std::fs::write(package.join("extension.toml"), HELLO_MANIFEST)?;

    let err = ExtensionHost::load(&root)
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    assert!(
        err.contains("without extension.wasm"),
        "expected a torn-package error, got {err:?}"
    );
    Ok(())
}
