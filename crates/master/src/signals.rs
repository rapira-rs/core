//! Master self-pipe: an async-signal-safe handler translates signals into
//! bytes on a nonblocking `AF_UNIX` socketpair that the poll loop drains. The
//! design uses a socketpair self-pipe with a `sigfillset` mask on the handler,
//! unblock-all after install, and a `getpid` last-resort guard (#76601).

use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

use libc::c_int;

/// Write end of the self-pipe, read by the async-signal-safe handler. `-1`
/// until install. Set once, before handlers are armed.
static SELF_PIPE_WR: AtomicI32 = AtomicI32::new(-1);
/// Master pid captured at install; the handler refuses to write from any other
/// process (a child that took a signal inside the fork window, #76601).
static MASTER_PID: AtomicI32 = AtomicI32::new(0);

/// Control bytes emitted by the handler, consumed by the poll loop.
pub(crate) const SIG_TERM: u8 = b'T';
pub(crate) const SIG_INT: u8 = b'I';
pub(crate) const SIG_USR1: u8 = b'1';
pub(crate) const SIG_USR2: u8 = b'2';
pub(crate) const SIG_QUIT: u8 = b'Q';
pub(crate) const SIG_CHLD: u8 = b'C';
pub(crate) const SIG_HUP: u8 = b'H';

/// The full master disposition set: installed in the master, reset in children.
pub(crate) const MASTER_SIGNALS: [c_int; 7] = [
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGQUIT,
    libc::SIGCHLD,
    libc::SIGHUP,
];

/// Owned ends of the self-pipe socketpair.
pub(crate) struct SelfPipe {
    pub rd: OwnedFd,
    pub wr: OwnedFd,
}

#[cfg(target_os = "linux")]
fn errno_location() -> *mut c_int {
    // SAFETY: libc provides the thread-local errno slot address.
    unsafe { libc::__errno_location() }
}
#[cfg(target_os = "macos")]
fn errno_location() -> *mut c_int {
    // SAFETY: libc provides the thread-local errno slot address.
    unsafe { libc::__error() }
}

pub(crate) fn errno_get() -> c_int {
    // SAFETY: reading the thread-local errno slot.
    unsafe { *errno_location() }
}

fn errno_set(v: c_int) {
    // SAFETY: writing the thread-local errno slot we just read.
    unsafe { *errno_location() = v }
}

/// Async-signal-safe: `getpid`, `write`, and errno save/restore only.
extern "C" fn master_sig_handler(signo: c_int) {
    // #76601: a child that caught a signal between fork() and its disposition
    // reset must never write into the master's pipe.
    // SAFETY: getpid is async-signal-safe.
    if unsafe { libc::getpid() } != MASTER_PID.load(Ordering::Relaxed) {
        return;
    }
    let byte: u8 = match signo {
        libc::SIGTERM => SIG_TERM,
        libc::SIGINT => SIG_INT,
        libc::SIGUSR1 => SIG_USR1,
        libc::SIGUSR2 => SIG_USR2,
        libc::SIGQUIT => SIG_QUIT,
        libc::SIGCHLD => SIG_CHLD,
        libc::SIGHUP => SIG_HUP,
        _ => return,
    };
    let saved = errno_get();
    let fd = SELF_PIPE_WR.load(Ordering::Relaxed);
    if fd >= 0 {
        // Nonblocking stream socket: EAGAIN on a full buffer drops the byte,
        // which is harmless — every byte is level-idempotent for the loop.
        // SAFETY: write to a valid fd from a 1-byte stack buffer.
        unsafe { libc::write(fd, (&raw const byte).cast(), 1) };
    }
    errno_set(saved);
}

pub(crate) fn set_nonblock(fd: RawFd) -> io::Result<()> {
    // SAFETY: F_GETFL/F_SETFL on a valid fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: setting O_NONBLOCK on a valid fd.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: F_GETFD/F_SETFD on a valid fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: setting FD_CLOEXEC on a valid fd.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Build a sigset from an explicit list (no ambient state).
pub(crate) fn sigset(sigs: &[c_int]) -> libc::sigset_t {
    // SAFETY: zeroed sigset_t is initialized in full by sigemptyset below.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: set points to a live sigset_t for the duration of these calls.
    unsafe {
        libc::sigemptyset(&mut set);
        for &s in sigs {
            libc::sigaddset(&mut set, s);
        }
    }
    set
}

fn sigprocmask(how: c_int, set: &libc::sigset_t) {
    // SAFETY: set is a live sigset_t; null old mask discards the previous set.
    unsafe { libc::sigprocmask(how, set, std::ptr::null_mut()) };
}

/// Block {USR1, USR2, CHLD, HUP} very early — before any handlers exist — so a
/// stray reload signal (USR1/USR2/HUP all map to Reload; default disposition:
/// terminate) cannot kill the process during boot.
pub fn block_early_signals() {
    let set = sigset(&[libc::SIGUSR1, libc::SIGUSR2, libc::SIGCHLD, libc::SIGHUP]);
    sigprocmask(libc::SIG_BLOCK, &set);
}

/// Install the master sigaction set on a fresh self-pipe, then unblock all
/// signals. Must run in the master after PHP MINIT: handlers exist only after
/// the engine is up, and Zend never touches them since the master never calls
/// `php_request_startup`.
pub(crate) fn install_master_signals() -> anyhow::Result<SelfPipe> {
    let mut sp = [0 as RawFd; 2];
    // AF_UNIX SOCK_STREAM self-pipe, both ends O_NONBLOCK + FD_CLOEXEC.
    // SAFETY: sp is a 2-element array the syscall fills with valid fds.
    anyhow::ensure!(
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sp.as_mut_ptr()) } == 0,
        "socketpair: {}",
        io::Error::last_os_error()
    );
    for fd in sp {
        set_nonblock(fd)?;
        set_cloexec(fd)?;
    }

    // SAFETY: getpid is always safe.
    MASTER_PID.store(unsafe { libc::getpid() }, Ordering::Relaxed);
    SELF_PIPE_WR.store(sp[1], Ordering::Relaxed);

    // SAFETY: standard sigaction install; act is fully initialized, mask is a
    // live sigset_t, null old-action pointer discards the previous handler.
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = master_sig_handler as *const () as usize;
        // Handler runs with all signals blocked, so it is never re-entered.
        libc::sigfillset(&mut act.sa_mask);
        act.sa_flags = 0;
        for sig in MASTER_SIGNALS {
            anyhow::ensure!(
                libc::sigaction(sig, &act, std::ptr::null_mut()) == 0,
                "sigaction({sig}): {}",
                io::Error::last_os_error()
            );
        }
    }

    // Undo the early-boot block: from here signals land in the handler.
    let mut all = sigset(&[]);
    // SAFETY: all is a live sigset_t.
    unsafe { libc::sigfillset(&mut all) };
    sigprocmask(libc::SIG_UNBLOCK, &all);

    Ok(SelfPipe {
        // SAFETY: sp holds two fresh fds we now take sole ownership of.
        rd: unsafe { OwnedFd::from_raw_fd(sp[0]) },
        wr: unsafe { OwnedFd::from_raw_fd(sp[1]) },
    })
}

/// Master pid recorded at install; children compare `getppid` against it.
pub(crate) fn master_pid() -> c_int {
    MASTER_PID.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigset_membership() {
        let set = sigset(&[libc::SIGTERM, libc::SIGQUIT]);
        // SAFETY: set is a live sigset_t.
        unsafe {
            assert_eq!(libc::sigismember(&set, libc::SIGTERM), 1);
            assert_eq!(libc::sigismember(&set, libc::SIGQUIT), 1);
            assert_eq!(libc::sigismember(&set, libc::SIGUSR1), 0);
            assert_eq!(libc::sigismember(&set, libc::SIGCHLD), 0);
        }
    }

    #[test]
    fn empty_sigset_has_no_members() {
        let set = sigset(&[]);
        // SAFETY: set is a live sigset_t.
        unsafe {
            for s in [libc::SIGTERM, libc::SIGINT, libc::SIGUSR1, libc::SIGCHLD] {
                assert_eq!(libc::sigismember(&set, s), 0);
            }
        }
    }

    #[test]
    fn errno_roundtrip() {
        errno_set(0);
        assert_eq!(errno_get(), 0);
        errno_set(libc::EAGAIN);
        assert_eq!(errno_get(), libc::EAGAIN);
        errno_set(0);
    }
}
