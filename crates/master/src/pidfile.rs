//! Pidfile guard: written with the master pid, unlinked by `Drop` on every
//! master exit path. Only the master ever holds one - children `_exit` without
//! running drops, so a worker can never unlink it.

use std::io;
use std::path::{Path, PathBuf};

pub struct PidFile {
    path: PathBuf,
}

impl PidFile {
    /// Write the current pid to `path`.
    pub fn write(path: &Path) -> io::Result<PidFile> {
        std::fs::write(path, format!("{}\n", std::process::id()))?;
        Ok(PidFile {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
