use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::anyhow;
use extension_api::{Extension, ListenAddr, Middleware, Php, PrepareCtx, PreparedListener, Result};
use tokio::runtime::{self, Builder};
use tokio::sync::{oneshot, watch};

mod bridge;
mod check;
mod handler;
mod request;
mod response;
mod serve;

#[derive(Clone)]
pub struct Config {
    pub listen: ListenAddr,
    pub server_name: String,
    pub server_port: u16,
    pub max_body_size: usize,
    pub unsafe_field_names: UnsafeFieldNames,
    pub superglobals: bool,
    pub write_timeout: Duration,
    pub drain_grace: Duration,
    pub keepalive_timeout: Duration,
    pub middleware: Vec<Arc<dyn Middleware>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsafeFieldNames {
    Drop,
    Reject,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: ListenAddr::Tcp(std::net::SocketAddr::from(([127, 0, 0, 1], 8000))),
            server_name: "localhost".to_owned(),
            server_port: 8000,
            max_body_size: 8 * 1024 * 1024,
            unsafe_field_names: UnsafeFieldNames::Drop,
            superglobals: true,
            write_timeout: Duration::from_secs(30),
            drain_grace: Duration::from_secs(25),
            keepalive_timeout: Duration::from_secs(60),
            middleware: Vec::new(),
        }
    }
}

pub struct HttpServer {
    config: Config,
    prepared: Option<PreparedListener>,
    shutdown: Option<watch::Sender<bool>>,
    join: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl Extension for HttpServer {
    type Config = Config;

    fn init(config: Config) -> Self {
        Self {
            config,
            prepared: None,
            shutdown: None,
            join: None,
        }
    }

    fn name(&self) -> &str {
        "rapira-tower"
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx) -> Result<()> {
        let prepared = match &self.config.listen {
            ListenAddr::Tcp(addr) => ctx.bind_tcp(*addr)?,
            ListenAddr::Unix(path) => ctx.bind_unix(path)?,
        };
        tracing::info!(target: "http", "prepared listener on {}", prepared.addr_string()?);
        self.prepared = Some(prepared);
        Ok(())
    }

    async fn run(&mut self, php: Php) -> Result<()> {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (done_tx, done_rx) = oneshot::channel();
        let config = self.config.clone();
        let prepared = self.prepared.take();

        let thread = std::thread::Builder::new()
            .name("rapira-tower".into())
            .spawn(move || {
                let rt: runtime::Runtime = match Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(2)
                    .thread_name("rapira-tower-io")
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = done_tx.send(());
                        return Err(anyhow!("building the http runtime: {e}"));
                    }
                };
                let result = rt.block_on(serve::serve(php, config, prepared, shutdown_rx));
                let _ = done_tx.send(());
                result
            })?;

        self.shutdown = Some(shutdown_tx);
        self.join = Some(tokio::task::spawn_blocking(move || join_thread(thread)));

        let _ = done_rx.await;

        let result = match self.join.as_mut() {
            Some(join) => join
                .await
                .map_err(|e| anyhow!("http join task failed: {e}"))?,
            None => Ok(()),
        };
        self.join = None;
        result
    }

    async fn shutdown(&mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(true);
        }
        if let Some(join) = self.join.take() {
            join.await
                .map_err(|e| anyhow!("http join task failed: {e}"))??;
        }
        Ok(())
    }
}

fn join_thread(thread: JoinHandle<Result<()>>) -> Result<()> {
    thread.join().map_err(|payload| {
        let msg = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("unknown panic");
        anyhow!("http server thread panicked: {msg}")
    })?
}
