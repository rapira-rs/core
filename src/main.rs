use clap::{Args, CommandFactory, Parser, Subcommand};
use extension_api::{ListenAddr, PrepareCtx};
use php_sys::{Mode, Rapira};
use rapira_config::{Listen, Overrides, RunMode, Scaling, Settings, UnsafeFieldNames};
use rapira_runtime::ExtensionRuntime;
use rapira_scoreboard::Scoreboard;
use rapira_tower::{Config as HttpConfig, HttpServer, UnsafeFieldNames as HttpUnsafeFieldNames};
use std::{
    fs::{OpenOptions, read_dir, remove_file},
    path::PathBuf,
};
use tracing::info;

mod logging;

mod worker;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "rapira", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Serve(ServeArgs),
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    #[arg(long)]
    processes: Option<usize>,
    #[arg(long, value_name = "MODE")]
    mode: Option<RunMode>,
    #[arg(long)]
    classic: bool,
    #[arg(long, value_name = "ADDR")]
    listen: Option<Listen>,
    #[arg(value_name = "SCRIPT")]
    script: Option<PathBuf>,
}

/// Signals are blocked first: USR1/USR2/HUP terminate by default until the master installs its handlers.
fn main() -> anyhow::Result<()> {
    rapira_master::block_early_signals();

    match Cli::parse().command {
        Some(Commands::Serve(args)) => serve(args),
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

/// kill(pid, 0) probes existence without signaling: ESRCH means the owner is gone, EPERM means it runs under another uid. https://man7.org/linux/man-pages/man2/kill.2.html
fn spool_dir_reclaimable(name: &str) -> bool {
    let Some(pid) = name
        .strip_prefix("rapira-spool-")
        .and_then(|p| p.parse::<i32>().ok())
        .filter(|&p| p > 0)
    else {
        return false;
    };
    let gone = unsafe { libc::kill(pid, 0) } == -1;
    gone && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn serve(args: ServeArgs) -> anyhow::Result<()> {
    if args.classic && args.mode.is_some_and(|m| m != RunMode::Classic) {
        anyhow::bail!("--classic conflicts with --mode; use --mode classic");
    }

    let settings: Settings = rapira_config::resolve(
        args.config.as_deref(),
        Overrides {
            listen: args.listen,
            processes: args.processes,
            mode: args.mode.or(args.classic.then_some(RunMode::Classic)),
            entrypoint: args.script,
        },
    )?;

    logging::init(&settings.log)?;
    info!(target: "rapira", "rapira_core v{} starting", env!("CARGO_PKG_VERSION"));

    let mode: Mode = match settings.pool.mode {
        RunMode::Classic => Mode::Classic,
        RunMode::Worker => Mode::Worker(settings.pool.entrypoint.clone()),
        RunMode::Dispatcher => Mode::Dispatcher(settings.pool.entrypoint.clone()),
    };

    // set sendfileroot + make it canonical (std::fs)
    php_sys::set_sendfile_root(
        settings
            .http
            .sendfile_root
            .clone()
            .or_else(|| {
                settings
                    .pool
                    .entrypoint
                    .parent()
                    .map(std::path::Path::to_path_buf)
            })
            .unwrap_or_else(|| PathBuf::from("/")),
    );

    // parse HTTP configuration -------------------------------------
    let http_cfg: HttpConfig = HttpConfig {
        listen: match settings.http.listen {
            Listen::Tcp(addr) => ListenAddr::Tcp(addr),
            Listen::Unix(path) => ListenAddr::Unix(path),
        },
        server_name: settings.http.server_name,
        server_port: settings.http.server_port,
        max_body_size: settings.http.max_body_size,
        write_timeout: settings.http.write_timeout,
        drain_grace: settings.supervisor.drain_grace(),
        unsafe_field_names: match settings.http.unsafe_field_names {
            UnsafeFieldNames::Drop => HttpUnsafeFieldNames::Drop,
            UnsafeFieldNames::Reject => HttpUnsafeFieldNames::Reject,
        },
        superglobals: !matches!(mode, Mode::Dispatcher(_)),
        keepalive_timeout: settings.http.keepalive_timeout,
        middleware: Vec::new(),
    };
    //----------------------------------------------------------------

    // uploads -------------------------------------------------------
    if matches!(mode, Mode::Dispatcher(_)) {
        std::fs::create_dir_all(&settings.http.uploads.dir).map_err(|e| {
            anyhow::anyhow!(
                "creating http.uploads.dir {}: {e}",
                settings.http.uploads.dir.display()
            )
        })?;
        let probe = settings
            .http
            .uploads
            .dir
            .join(format!(".rapira-probe-{}", std::process::id()));
        let _ = remove_file(&probe);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .map_err(|e| {
                anyhow::anyhow!(
                    "http.uploads.dir {} is not writable: {e}",
                    settings.http.uploads.dir.display()
                )
            })?;
        let _ = remove_file(&probe);
        match read_dir(&settings.http.uploads.dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    if !spool_dir_reclaimable(&entry.file_name().to_string_lossy()) {
                        continue;
                    }
                    let path = entry.path();
                    if let Err(e) = std::fs::remove_dir_all(&path) {
                        tracing::warn!(target: "rapira", "sweeping spool dir {}: {e}", path.display());
                    }
                }
            }
            Err(e) => {
                tracing::warn!(target: "rapira", "listing {} for the spool sweep: {e}", settings.http.uploads.dir.display());
            }
        }
    }
    let upload_limits = rapira_runtime::multipart::Limits {
        dir: settings.http.uploads.dir.clone(),
        max_file_size: settings.http.uploads.max_file_size,
        max_field_size: settings.http.uploads.max_field_size,
        max_files: settings.http.uploads.max_files,
        max_parts: settings.http.uploads.max_parts,
        max_part_headers: settings.http.uploads.max_part_headers,
    };

    // ---------------------------------------------------------------------

    let mut host: ExtensionRuntime = ExtensionRuntime::new();
    host.register::<HttpServer>(http_cfg)?;

    let mut prepare_ctx: PrepareCtx = PrepareCtx::new();
    host.prepare_all(&mut prepare_ctx)?;
    let listeners: Vec<i32> = prepare_ctx.listener_fds().to_vec();

    let module: php_sys::PhpModule = Rapira::boot_master()?;

    anyhow::ensure!(
        settings.pool.processes <= rapira_scoreboard::SB_MAX_SLOTS / 2,
        "pool.processes ({}) exceeds the supported maximum ({})",
        settings.pool.processes,
        rapira_scoreboard::SB_MAX_SLOTS / 2
    );
    let scoreboard: Scoreboard = Scoreboard::create(settings.pool.processes * 2)?;

    // forks ------------------------------------------------------------------
    let cfg: rapira_master::MasterConfig = rapira_master::MasterConfig {
        processes: settings.pool.processes,
        scaling: match settings.pool.scaling {
            Scaling::Static => rapira_master::Scaling::Static,
            Scaling::Dynamic {
                min_spare,
                max_spare,
            } => rapira_master::Scaling::Dynamic {
                min_spare,
                max_spare,
            },
            Scaling::Ondemand => rapira_master::Scaling::Ondemand,
        },
        process_idle_timeout: settings.pool.process_idle_timeout,
        process_control_timeout: settings.supervisor.process_control_timeout,
        request_terminate_timeout: settings.pool.request_terminate_timeout,
        pidfile: settings.supervisor.pidfile.clone(),
        listeners,
    };

    let mut host_cell: Option<ExtensionRuntime> = Some(host);
    let stop: Result<rapira_master::StopReason, anyhow::Error> =
        rapira_master::run(cfg, scoreboard, move |env: rapira_master::WorkerEnv| {
            let host: ExtensionRuntime = host_cell.take().expect("fresh child owns the host copy");
            worker::worker_body(
                env,
                host,
                mode.clone(),
                settings.pool.entrypoint.clone(),
                settings.pool.max_requests,
                upload_limits.clone(),
                settings.supervisor.process_control_timeout,
            )
        });

    match stop {
        Ok(rapira_master::StopReason::Drained) => {
            drop(module);
            Ok(())
        }
        Ok(rapira_master::StopReason::Forced) => std::process::exit(130),
        Err(e) => {
            tracing::error!(target: "rapira", "master failed: {e:#}");
            std::process::exit(rapira_master::MASTER_EXIT_FAILBOOT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::spool_dir_reclaimable;

    /// The sweep reclaims only dirs whose owning process is gone.
    #[test]
    fn spool_sweep_reclaims_only_dead_pid_dirs() {
        assert!(!spool_dir_reclaimable("other-dir"));
        assert!(!spool_dir_reclaimable("rapira-spool-"));
        assert!(!spool_dir_reclaimable("rapira-spool-x"));
        assert!(!spool_dir_reclaimable("rapira-spool--5"));
        assert!(!spool_dir_reclaimable("rapira-spool-0"));
        let live = std::process::id();
        assert!(!spool_dir_reclaimable(&format!("rapira-spool-{live}")));

        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a short-lived child");
        let dead = child.id();
        let _ = child.wait();
        assert!(spool_dir_reclaimable(&format!("rapira-spool-{dead}")));
    }
}
