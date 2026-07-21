//! Pidfile guard. Written after listeners bind so it only exists once the
//! master is ready to supervise, unlinked explicitly on clean/forced stop with
//! a `Drop` backstop for error paths. Only the master ever holds one — children
//! `_exit` without running drops, so a worker can never unlink it.

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

    /// Remove the pidfile now (idempotent).
    pub fn unlink(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        self.unlink();
    }
}
