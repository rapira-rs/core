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

    // Boot PHP; extensions (if any) drive requests through the pool via `exec`.
    let rapira = Rapira::start(Mode::Classic, 1)?;

    // Run the extensions; they drive PHP through the pool via `exec`. The guard
    // joins the driver threads (dropping their handles) before `rapira` drops.
    let running = ext.run(rapira.handle()?);

    drop(running);
    drop(rapira);
    Ok(())
}
