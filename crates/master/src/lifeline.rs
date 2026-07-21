//! Master-death backstop. One pipe whose write end the master holds forever and
//! never writes; each worker keeps the read end and closes its inherited write
//! end. When the master dies every write end is gone, so the worker's read end
//! sees EOF and drains. Linux additionally arms `PR_SET_PDEATHSIG` for
//! promptness (reliable because the master is single-threaded).

use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

#[cfg(not(target_os = "linux"))]
use crate::signals::set_cloexec;

/// Both ends of the lifeline pipe, master-owned. `wr` is the token whose
/// disappearance signals master death to every worker's `rd`.
pub struct Lifeline {
    pub rd: OwnedFd,
    pub wr: OwnedFd,
}

impl Lifeline {
    /// Create the pipe with both ends `CLOEXEC` (fork inherits fds regardless of
    /// CLOEXEC; the flag only keeps them out of exec'd processes). Linux uses
    /// `pipe2` in a single syscall; other platforms fall back to `pipe` + `fcntl`.
    pub fn create() -> anyhow::Result<Lifeline> {
        let mut fds = [0 as RawFd; 2];
        #[cfg(target_os = "linux")]
        // SAFETY: fds is a 2-element array the syscall fills with valid fds.
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        #[cfg(not(target_os = "linux"))]
        let rc = {
            // SAFETY: fds is a 2-element array the syscall fills with valid fds.
            let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
            if r == 0 {
                set_cloexec(fds[0])?;
                set_cloexec(fds[1])?;
            }
            r
        };
        anyhow::ensure!(rc == 0, "lifeline pipe: {}", io::Error::last_os_error());
        Ok(Lifeline {
            // SAFETY: fds holds two fresh fds we take sole ownership of.
            rd: unsafe { OwnedFd::from_raw_fd(fds[0]) },
            wr: unsafe { OwnedFd::from_raw_fd(fds[1]) },
        })
    }

    /// Duplicate the read end for a worker. The worker owns this copy; the
    /// master keeps its originals. The dup carries no state the child shares.
    pub fn dup_read_end(&self) -> io::Result<OwnedFd> {
        self.rd.try_clone()
    }
}
