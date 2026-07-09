use extension_host::ExtensionHost;
use php_sys::{Mode, Rapira};
use std::path::PathBuf;

/// Where extensions are installed. Resolved to an absolute path before anything
/// daemonizes, since a daemon's cwd is not the deploy directory.
fn ext_dir() -> anyhow::Result<PathBuf> {
    let dir = std::env::var_os("RAPIRA_EXT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("extensions"));

    Ok(std::path::absolute(&dir)?)
}

fn main() -> anyhow::Result<()> {
    // Validate every extension before booting PHP; a bad package fails here.
    let dir = ext_dir()?;
    let ext = ExtensionHost::load(&dir)?;
    if ext.is_empty() {
        eprintln!(
            "[rapira] no extensions in {}; nothing to run",
            dir.display()
        );
        return Ok(());
    }

    // The exec path is Worker mode: a resident script answers each exec via
    // rapira_handle_request. Classic mode would open the exec URI path as a script
    // file, so extensions would never reach their handler.
    let script = std::env::var_os("RAPIRA_WORKER_SCRIPT")
        .map(PathBuf::from)
        .ok_or_else(|| {
            anyhow::anyhow!("RAPIRA_WORKER_SCRIPT must point at the resident worker script")
        })?;
    let rapira = Rapira::start(Mode::Worker(std::path::absolute(&script)?), 1)?;

    // join() drops the driver handles (and their RapiraHandle clones) before we drop
    // `rapira`, and surfaces a failed extension as a non-zero exit.
    let outcomes = ext.run(rapira.handle()?).join();
    drop(rapira);
    for outcome in outcomes {
        outcome.map_err(|msg| anyhow::anyhow!("extension failed: {msg}"))?;
    }
    Ok(())
}
