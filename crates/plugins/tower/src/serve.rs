use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::anyhow;
use extension_api::{Addr, ListenAddr, Php, PrepareCtx, PreparedListener, Result};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::watch::{self, channel};

use crate::Config;
use crate::handler::{ConnInfo, RapiraService, Shared};

enum Acceptor {
    Tcp(TcpListener),
    Unix(UnixListener),
}

fn create_acceptor(prepared: Option<PreparedListener>, listen: &ListenAddr) -> Result<Acceptor> {
    use std::os::fd::{FromRawFd, IntoRawFd};
    let prepared = match prepared {
        Some(p) => p,
        None => match listen {
            ListenAddr::Tcp(addr) => PrepareCtx::new().bind_tcp(*addr)?,
            ListenAddr::Unix(path) => PrepareCtx::new().bind_unix(path)?,
        },
    };
    let tcp: bool = matches!(prepared.addr(), ListenAddr::Tcp(_));
    // SAFETY: into_raw_fd transfers sole ownership of a listening socket; prepare
    // already set O_NONBLOCK, which from_std requires but does not set.
    if tcp {
        let std = unsafe { std::net::TcpListener::from_raw_fd(prepared.into_raw_fd()) };
        Ok(Acceptor::Tcp(TcpListener::from_std(std)?))
    } else {
        let std = unsafe { std::os::unix::net::UnixListener::from_raw_fd(prepared.into_raw_fd()) };
        Ok(Acceptor::Unix(UnixListener::from_std(std)?))
    }
}

pub(crate) async fn serve(
    php: Php,
    config: Config,
    prepared: Option<PreparedListener>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let acceptor = create_acceptor(prepared, &config.listen)?;
    match &config.listen {
        ListenAddr::Tcp(a) => tracing::info!(target: "http", "listening on http://{a}"),
        ListenAddr::Unix(p) => tracing::info!(target: "http", "listening on unix:{}", p.display()),
    }

    let chain: Arc<[_]> = config.middleware.clone().into();
    let cfg = Arc::new(config);
    let inflight: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let shared = Arc::new(Shared {
        cfg: Arc::clone(&cfg),
        php,
        chain,
        inflight: Arc::clone(&inflight),
    });
    let graceful = GracefulShutdown::new();

    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(cfg.keepalive_timeout)
        .preserve_header_case(false)
        .half_close(false)
        .keep_alive(true);
    let builder = Arc::new(builder);

    let mut fatal: Option<anyhow::Error> = None;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => break,
            res = accept_connection(&acceptor, &cfg.listen, &builder, &graceful, &shared) => match res {
                Ok(()) => {}
                Err(e) if is_fatal_accept(&e) => {
                    fatal = Some(anyhow!("listener failed: {e}"));
                    break;
                }
                Err(e) if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::Interrupted
                ) => {
                    tracing::debug!(target: "http", "accept skipped: {e}");
                }
                Err(e) => {
                    tracing::warn!(target: "http", "accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    drop(acceptor);
    let deadline = tokio::time::Instant::now() + cfg.drain_grace;
    if tokio::time::timeout_at(deadline, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            target: "http",
            "graceful connection shutdown did not finish within {:?}",
            cfg.drain_grace
        );
    }
    while inflight.load(Ordering::Acquire) > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stranded = inflight.load(Ordering::Acquire);
    if let Some(e) = fatal {
        if stranded > 0 {
            tracing::warn!(
                target: "http",
                "{stranded} request(s) still in flight when the listener failed"
            );
        }
        return Err(e);
    }
    if stranded > 0 {
        return Err(anyhow!(
            "http drain timed out after {:?} with {stranded} request(s) in flight; \
             their responses were cut short",
            cfg.drain_grace
        ));
    }
    tracing::info!(target: "http", "drained cleanly; accept loop stopped");
    Ok(())
}

fn listen_addr(listen: &ListenAddr) -> Addr {
    match listen {
        ListenAddr::Tcp(a) => Addr::Inet(*a),
        ListenAddr::Unix(p) => Addr::Unix(Some(p.clone())),
    }
}

fn is_fatal_accept(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK | libc::EOPNOTSUPP)
    )
}

async fn accept_connection(
    acceptor: &Acceptor,
    listen: &ListenAddr,
    builder: &Arc<http1::Builder>,
    graceful: &GracefulShutdown,
    shared: &Arc<Shared>,
) -> std::io::Result<()> {
    match acceptor {
        Acceptor::Tcp(l) => {
            let (stream, peer) = l.accept().await?;
            let _ = stream.set_nodelay(true);
            let server = stream
                .local_addr()
                .map(Addr::Inet)
                .unwrap_or_else(|_| listen_addr(listen));
            spawn_conn(stream, Addr::Inet(peer), server, builder, graceful, shared);
        }
        Acceptor::Unix(l) => {
            let (stream, peer) = l.accept().await?;
            let remote = Addr::Unix(peer.as_pathname().map(Into::into));
            let server = listen_addr(listen);
            spawn_conn(stream, remote, server, builder, graceful, shared);
        }
    }
    Ok(())
}

fn spawn_conn<S>(
    stream: S,
    remote: Addr,
    server: Addr,
    builder: &Arc<http1::Builder>,
    graceful: &GracefulShutdown,
    shared: &Arc<Shared>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (closed_tx, closed_rx) = channel(false);
    let conn = ConnInfo {
        remote,
        server,
        closed: closed_rx,
    };

    let svc = RapiraService::new(Arc::clone(shared), conn);
    let io = crate::bridge::TimedIo::new(TokioIo::new(stream), shared.cfg.write_timeout);
    // connection <I, S>
    let connection = builder.serve_connection(io, svc);
    let watched = graceful.watch(connection);
    tokio::spawn(async move {
        if let Err(e) = watched.await {
            tracing::debug!(target: "http", "connection ended with error: {e}");
        }
        let _ = closed_tx.send(true);
    });
}
