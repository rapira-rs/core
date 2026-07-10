use clap::{Parser, ValueEnum};
use extension_host::ExtensionHost;
use php_sys::{Mode, Rapira};
use rapira_http::HttpServer;
use std::path::PathBuf;

// Profiling shows ~50% of CPU under load in glibc malloc/free + arena contention across the
// worker/IO threads; the Rust-side per-request churn (marshaling, CStrings, buffers) dominates.
// mimalloc benchmarked fastest here (66.6k vs jemalloc 61k vs glibc 58.5k) — best fit for the
// many-small-allocations-across-many-threads profile; it cuts alloc cost and arena contention.
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
    let cli = Cli::parse();
    // Resolve to an absolute path before anything daemonizes; a daemon's cwd is not
    // the deploy directory.
    let script = std::path::absolute(&cli.script)?;

    // Extensions are compiled in; register the HTTP front (and any others) here. With
    // none registered there is nothing to serve, so exit before booting PHP.
    let mut host = ExtensionHost::new();
    host.register::<HttpServer>()?;
    if host.is_empty() {
        return Ok(());
    }

    // Block SIGINT/SIGTERM before spawning any threads (PHP workers, the extension
    // runtime), so rapira reaps them on a dedicated waiter and drains extensions on
    // shutdown — instead of a signal handler that would fight Zend's per-request one.
    extension_host::arm_shutdown_signals();

    let mode = match cli.mode {
        ModeArg::Classic => Mode::Classic,
        ModeArg::Worker => Mode::Worker(script.clone()),
    };
    let rapira = Rapira::start(mode, cli.threads)?;

    // Extensions drive PHP through the pool via `php`, running `script`. serve() runs
    // until they finish or a SIGTERM/SIGINT arrives, drives each extension's shutdown,
    // and drops the Php/RapiraHandle clones before we drop `rapira` (shutdown
    // contract). A failed extension is a non-zero exit.
    let outcomes = host.run(rapira.handle()?, script).serve();
    drop(rapira);
    for outcome in outcomes {
        outcome.map_err(|msg| anyhow::anyhow!("extension failed: {msg}"))?;
    }
    Ok(())
}
