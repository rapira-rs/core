use clap::{Args, CommandFactory, Parser, Subcommand};
use extension_host::ExtensionHost;
use log::info;
use php_sys::{Mode, Rapira};
use rapira_config::{Listen, Overrides, Settings};
use rapira_pingora::{Config as HttpConfig, HttpServer, Listen as HttpListen};
use std::path::PathBuf;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// PHP application server driven by native extensions.
#[derive(Parser)]
#[command(name = "rapira", version)]
struct Cli {
    // Optional so a bare `rapira` prints help, and so future top-level forms (a naked
    // `rapira <script.php>` positional, a `rapira run` subcommand) slot in without
    // breaking `serve`.
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Boot the server: start PHP, register extensions, and serve requests.
    Serve(ServeArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Load settings from a rapira.toml. The flags below override values it sets.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// PHP worker threads. Rapira runs one PHP thread; values > 1 are ignored with a warning.
    #[arg(long)]
    threads: Option<usize>,

    /// Re-include the script for every request instead of keeping it resident.
    #[arg(long)]
    classic: bool,

    /// Listen address: `host:port`, `:port` (all interfaces), or `unix:<path>`.
    #[arg(long, value_name = "ADDR")]
    listen: Option<Listen>,

    /// PHP entry script; overrides `pool.entrypoint` from the config file.
    #[arg(value_name = "SCRIPT")]
    script: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    info!(target: "rapira", "rapira_core v{} starting", env!("CARGO_PKG_VERSION"));

    match Cli::parse().command {
        Some(Commands::Serve(args)) => serve(args),
        // Bare `rapira`: nothing to run, so show usage and exit cleanly.
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

fn serve(args: ServeArgs) -> anyhow::Result<()> {
    // Collapse CLI flags, the config file, and defaults into one validated struct. This
    // also resolves the entry script to an absolute path before anything daemonizes; a
    // daemon's cwd is not the deploy directory.
    let settings: Settings = rapira_config::resolve(
        args.config.as_deref(),
        Overrides {
            listen: args.listen,
            threads: args.threads,
            classic: args.classic,
            entrypoint: args.script,
        },
    )?;
    let script: PathBuf = settings.pool.entrypoint.clone();

    // Block SIGINT/SIGTERM before spawning any threads (PHP workers, the extension
    // runtime, anything an extension's constructor may create), so rapira reaps them on
    // a dedicated waiter and drains extensions on shutdown - instead of a signal
    // handler that would fight Zend's per-request one.
    extension_host::arm_shutdown_signals();

    // rapira_config::Listen and rapira_pingora::Listen are distinct types on purpose: the
    // extension crate stays independent of core's config crate, and core owns the one
    // mapping between them (a From impl is barred by the orphan rule anyway).
    let http_cfg = HttpConfig {
        listen: match settings.http.listen {
            Listen::Tcp(addr) => HttpListen::Tcp(addr),
            Listen::Unix(path) => HttpListen::Unix(path),
        },
        server_name: settings.http.server_name,
        server_port: settings.http.server_port,
        max_body_size: settings.http.max_body_size,
    };

    // Extensions are compiled in; register the HTTP front (and any others) here, each
    // with its config. With none registered there is nothing to serve, so exit before
    // booting PHP.
    let mut host: ExtensionHost = ExtensionHost::new();
    host.register::<HttpServer>(http_cfg)?;
    if host.is_empty() {
        return Ok(());
    }

    // Both forms run the same worker pool; --classic only changes whether the script is
    // re-included per request (Classic) or stays resident (Worker). Either way the script
    // is also handed to host.run below, where the backend derives SCRIPT_FILENAME /
    // DOCUMENT_ROOT / SCRIPT_NAME from it.
    let mode = if settings.pool.classic {
        Mode::Classic
    } else {
        Mode::Worker(script.clone())
    };
    let rapira = Rapira::start(mode, settings.pool.threads)?;

    let outcomes = host.run(rapira.handle()?, script).serve();
    drop(rapira);
    for outcome in outcomes {
        outcome.map_err(|msg| anyhow::anyhow!("extension failed: {msg}"))?;
    }
    Ok(())
}
