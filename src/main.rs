use clap::{Parser, ValueEnum};
use extension_host::ExtensionHost;
use php_sys::{Mode, Rapira};
use rapira_http::HttpServer;
use std::path::PathBuf;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// PHP application server driven by native extensions.
#[derive(Parser)]
#[command(name = "rapira")]
struct Cli {
    /// How PHP runs each request: `classic` (one fresh request per exec) or `worker`
    /// (a resident script loops on rapira_handle_request).
    #[arg(long, value_enum, default_value_t = ModeArg::Worker)]
    mode: ModeArg,

    /// PHP entry script: the front controller (index.php) in classic mode, or the
    /// resident worker script in worker mode.
    #[arg(long)]
    script: PathBuf,

    /// Number of PHP worker threads (ZTS only; NTS always uses 1).
    #[arg(long, default_value_t = 1)]
    threads: usize,
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Classic,
    Worker,
}

fn main() -> anyhow::Result<()> {
    let cli: Cli = Cli::parse();
    // Resolve to an absolute path before anything daemonizes; a daemon's cwd is not
    // the deploy directory.
    let script: PathBuf = std::path::absolute(&cli.script)?;

    // Block SIGINT/SIGTERM before spawning any threads (PHP workers, the extension
    // runtime, anything an extension's init may create), so rapira reaps them on a
    // dedicated waiter and drains extensions on shutdown - instead of a signal
    // handler that would fight Zend's per-request one.
    extension_host::arm_shutdown_signals();

    // Extensions are compiled in; register the HTTP front (and any others) here. With
    // none registered there is nothing to serve, so exit before booting PHP.
    let mut host: ExtensionHost = ExtensionHost::new();
    host.register::<HttpServer>()?;
    if host.is_empty() {
        return Ok(());
    }

    let mode = match cli.mode {
        ModeArg::Classic => Mode::Classic,
        ModeArg::Worker => Mode::Worker(script.clone()),
    };
    let rapira = Rapira::start(mode, cli.threads)?;

    let outcomes = host.run(rapira.handle()?, script).serve();
    drop(rapira);
    for outcome in outcomes {
        outcome.map_err(|msg| anyhow::anyhow!("extension failed: {msg}"))?;
    }
    Ok(())
}
