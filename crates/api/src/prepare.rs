//! Master-side pre-fork resource preparation. Extensions bind their listen
//! sockets here - synchronously, before any fork and before any runtime
//! exists - and the bound fds are inherited by every forked worker.

use std::net::SocketAddr;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

/// Listen backlog for every pre-fork bind. Matches pingora-core's
/// LISTENER_BACKLOG: pingora re-listens with its own value when it adopts the
/// fd (on Linux a re-listen just updates the backlog), so any other default
/// would be silently rewritten in-worker.
pub const LISTEN_BACKLOG: i32 = 65535;

/// Bind address of a prepared listener. `addr_string()` is the canonical
/// string for BOTH pingora's endpoint (`add_tcp`/`add_uds`) and its `Fds` key -
/// pingora adopts only on an exact match, and a mismatch silently rebinds
/// (for unix sockets: unlinks and steals the master's socket). Derive both
/// strings from this one method, never by hand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenAddr {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

impl ListenAddr {
    pub fn addr_string(&self) -> anyhow::Result<String> {
        match self {
            ListenAddr::Tcp(a) => Ok(a.to_string()),
            ListenAddr::Unix(p) => p
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("unix socket path must be valid UTF-8")),
        }
    }
}

/// A bound, listening socket created in the master and inherited by every
/// forked worker (fork copies fds regardless of CLOEXEC; the CLOEXEC set here
/// matters only for a future re-exec reload, which must clear it first).
///
/// Ownership: exactly one closer per process. In the MASTER the listener stays
/// inside its extension for the master's whole life - respawned workers must
/// inherit it again. In a WORKER, `run` transfers the child's copy to the
/// adopter via `into_raw_fd` (pingora closes it at teardown).
#[derive(Debug)]
pub struct PreparedListener {
    fd: OwnedFd,
    addr: ListenAddr,
}

impl PreparedListener {
    pub fn addr(&self) -> &ListenAddr {
        &self.addr
    }

    pub fn addr_string(&self) -> anyhow::Result<String> {
        self.addr.addr_string()
    }
}

impl AsRawFd for PreparedListener {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl IntoRawFd for PreparedListener {
    fn into_raw_fd(self) -> RawFd {
        self.fd.into_raw_fd()
    }
}

/// Master-side binding context handed to `Extension::prepare`. Sync syscalls
/// only; one per boot.
pub struct PrepareCtx {
    backlog: i32,
    bound: Vec<ListenAddr>,
    fds: Vec<OwnedFd>,
}

impl Default for PrepareCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl PrepareCtx {
    pub fn new() -> Self {
        Self {
            backlog: LISTEN_BACKLOG,
            bound: Vec::new(),
            fds: Vec::new(),
        }
    }

    /// Every address bound so far (boot log / diagnostics).
    pub fn bound(&self) -> &[ListenAddr] {
        &self.bound
    }

    /// Raw fds of every listener bound so far, for the master's ondemand poll
    /// set. Backed by dups owned by this context, so they stay valid for the
    /// context's lifetime even if an extension drops its `PreparedListener`.
    pub fn listener_fds(&self) -> Vec<RawFd> {
        self.fds.iter().map(|fd| fd.as_raw_fd()).collect()
    }

    /// socket(STREAM|CLOEXEC) → SO_REUSEADDR → bind → listen → O_NONBLOCK.
    /// The returned listener carries the RESOLVED address (port 0 becomes real).
    /// Nonblocking is set here because pingora's adoption path hands the fd
    /// straight to tokio, which requires it.
    pub fn bind_tcp(&mut self, addr: SocketAddr) -> anyhow::Result<PreparedListener> {
        let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))
            .with_context(|| format!("socket for {addr}"))?;
        socket.set_reuse_address(true)?;
        socket
            .bind(&addr.into())
            .with_context(|| format!("bind {addr}"))?;
        socket
            .listen(self.backlog)
            .with_context(|| format!("listen {addr}"))?;
        socket.set_nonblocking(true)?;
        let resolved = socket
            .local_addr()?
            .as_socket()
            .expect("inet socket has an inet local addr");
        let addr = ListenAddr::Tcp(resolved);
        let dup = socket.try_clone().context("dup listener fd")?;
        self.record(addr.clone(), dup.into())?;
        Ok(PreparedListener {
            fd: socket.into(),
            addr,
        })
    }

    /// probe → unlink-stale → bind → listen → chmod 0o666 → O_NONBLOCK.
    /// 0o666 matches pingora's fresh-bind default and its adopt-branch
    /// re-chmod, so permissions are stable across bind and adoption.
    pub fn bind_unix(&mut self, path: &Path) -> anyhow::Result<PreparedListener> {
        // Never unlink a live socket: another instance may be serving on it.
        // A nonblocking connect distinguishes live (success, or WouldBlock on
        // a full backlog) from stale (ConnectionRefused) or absent (NotFound).
        let probe = Socket::new(Domain::UNIX, Type::STREAM, None)?;
        probe.set_nonblocking(true)?;
        match probe.connect(&SockAddr::unix(path)?) {
            Ok(()) => {
                anyhow::bail!("another server is already listening on {}", path.display())
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                anyhow::bail!("another server is already listening on {}", path.display())
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) => {}
            Err(e) => {
                return Err(e).with_context(|| format!("probing {}", path.display()));
            }
        }
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("removing stale socket {}", path.display()));
            }
        }
        let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
        socket
            .bind(&SockAddr::unix(path)?)
            .with_context(|| format!("bind unix:{}", path.display()))?;
        socket.listen(self.backlog)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
        socket.set_nonblocking(true)?;
        let addr = ListenAddr::Unix(path.to_owned());
        let dup = socket.try_clone().context("dup listener fd")?;
        self.record(addr.clone(), dup.into())?;
        Ok(PreparedListener {
            fd: socket.into(),
            addr,
        })
    }

    fn record(&mut self, addr: ListenAddr, fd: OwnedFd) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.bound.contains(&addr),
            "duplicate listener: {addr:?} already prepared"
        );
        self.bound.push(addr);
        self.fds.push(fd);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn tcp_bind_resolves_and_accepts() {
        let mut ctx = PrepareCtx::new();
        let l = ctx.bind_tcp("127.0.0.1:0".parse().unwrap()).unwrap();
        let ListenAddr::Tcp(resolved) = *l.addr() else {
            panic!()
        };
        assert_ne!(resolved.port(), 0);

        // Nonblocking + CLOEXEC flags are set on the fd.
        let flags = unsafe { libc::fcntl(l.as_raw_fd(), libc::F_GETFL) };
        assert!(flags & libc::O_NONBLOCK != 0, "O_NONBLOCK expected");
        let fdflags = unsafe { libc::fcntl(l.as_raw_fd(), libc::F_GETFD) };
        assert!(fdflags & libc::FD_CLOEXEC != 0, "FD_CLOEXEC expected");

        // The queue really accepts: connect, then accept via a blocking clone.
        let mut client = std::net::TcpStream::connect(resolved).unwrap();
        let std_l: std::net::TcpListener = {
            use std::os::fd::{FromRawFd, IntoRawFd};
            unsafe { std::net::TcpListener::from_raw_fd(l.into_raw_fd()) }
        };
        std_l.set_nonblocking(false).unwrap();
        let (mut srv, _) = std_l.accept().unwrap();
        client.write_all(b"x").unwrap();
        let mut b = [0u8; 1];
        srv.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"x");
    }

    #[test]
    fn unix_bind_sets_perms_and_reclaims_stale() {
        let dir = std::env::temp_dir().join(format!("rapira-prep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sock");

        for _ in 0..2 {
            // second pass proves stale-socket reclaim
            let mut ctx = PrepareCtx::new();
            let l = ctx.bind_unix(&path).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o666);
            drop(l);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unix_bind_refuses_live_socket() {
        let dir = std::env::temp_dir().join(format!("rapira-prep-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("live.sock");

        let mut ctx = PrepareCtx::new();
        let _live = ctx.bind_unix(&path).unwrap();
        let mut second = PrepareCtx::new();
        let err = second.bind_unix(&path).unwrap_err();
        assert!(err.to_string().contains("already listening"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_binds_rejected() {
        let mut ctx = PrepareCtx::new();
        let l = ctx.bind_tcp("127.0.0.1:0".parse().unwrap()).unwrap();
        let ListenAddr::Tcp(resolved) = *l.addr() else {
            panic!()
        };
        assert!(ctx.bind_tcp(resolved).is_err());
    }
}
