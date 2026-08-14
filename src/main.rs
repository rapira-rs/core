use clap::{Args, CommandFactory, Parser, Subcommand};
use extension_api::PrepareCtx;
use php_sys::{Mode, Rapira};
use rapira_config::{Listen, Overrides, PoolMode, Settings, UnsafeFieldNames};
use rapira_pingora::{
    Config as HttpConfig, HttpServer, Listen as HttpListen,
    UnsafeFieldNames as HttpUnsafeFieldNames,
};
use rapira_runtime::ExtensionRuntime;
use rapira_scoreboard::Scoreboard;
use std::path::PathBuf;
use tracing::info;

mod logging;

mod worker;

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

    /// Worker processes to fork (static count; max_children for `pool.mode`
    /// dynamic/ondemand). Defaults to the CPU count.
    #[arg(long)]
    processes: Option<usize>,

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
    // First statement: USR1/USR2/HUP default to terminate, and no handler
    // exists until the master installs its own. Harmless on non-serve paths.
    rapira_master::block_early_signals();

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

/// A `rapira-spool-<pid>` entry is reclaimable only when its owner is gone:
/// kill(pid, 0) probes existence without signaling, and ESRCH means no such
/// process. EPERM means it exists under another uid — not ours to sweep.
/// https://man7.org/linux/man-pages/man2/kill.2.html
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
    // Collapse CLI flags, the config file, and defaults into one validated struct. This
    // also resolves the entry script to an absolute path before anything daemonizes; a
    // daemon's cwd is not the deploy directory.
    let settings: Settings = rapira_config::resolve(
        args.config.as_deref(),
        Overrides {
            listen: args.listen,
            processes: args.processes,
            classic: args.classic,
            entrypoint: args.script,
        },
    )?;

    // The logger goes up the moment the config exists, and not before: the
    // config owns both the filter and the format, and the global logger installs
    // only once. Nothing earlier logs — signal blocking is syscalls, clap writes
    // its own usage/errors to stderr, and `resolve` reports failure by returning
    // it (a config error cannot be rendered in the format that failed to parse).
    // Non-serve paths never install a logger; stray `log::` calls are dropped.
    logging::init(&settings.log)?;
    info!(target: "rapira", "rapira_core v{} starting", env!("CARGO_PKG_VERSION"));

    let script: PathBuf = settings.pool.entrypoint.clone();

    // rapira_config::Listen and rapira_pingora::Listen are distinct types on purpose: the
    // extension crate stays independent of core's config crate, and core owns the one
    // mapping between them (a From impl is barred by the orphan rule anyway).
    let http_cfg: HttpConfig = HttpConfig {
        listen: match settings.http.listen {
            Listen::Tcp(addr) => HttpListen::Tcp(addr),
            Listen::Unix(path) => HttpListen::Unix(path),
        },
        server_name: settings.http.server_name,
        server_port: settings.http.server_port,
        max_body_size: settings.http.max_body_size,
        unsafe_field_names: match settings.http.unsafe_field_names {
            UnsafeFieldNames::Drop => HttpUnsafeFieldNames::Drop,
            UnsafeFieldNames::Reject => HttpUnsafeFieldNames::Reject,
        },
        // the Drop screen protects the $_SERVER mapping, which only classic
        // mode builds
        superglobals: settings.pool.classic,
    };

    // Spool boot: the root must take a file now, not fail per-request; sweep
    // spool dirs whose owning process is gone. The dir may be shared (the
    // default is the system temp dir), so liveness gates the sweep — another
    // running instance's dirs stay.
    let uploads = &settings.http.uploads;
    std::fs::create_dir_all(&uploads.dir)
        .map_err(|e| anyhow::anyhow!("creating http.uploads.dir {}: {e}", uploads.dir.display()))?;
    let probe = uploads
        .dir
        .join(format!(".rapira-probe-{}", std::process::id()));
    std::fs::write(&probe, b"").map_err(|e| {
        anyhow::anyhow!(
            "http.uploads.dir {} is not writable: {e}",
            uploads.dir.display()
        )
    })?;
    let _ = std::fs::remove_file(&probe);
    match std::fs::read_dir(&uploads.dir) {
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
            tracing::warn!(target: "rapira", "listing {} for the spool sweep: {e}", uploads.dir.display());
        }
    }
    let upload_limits = rapira_runtime::multipart::Limits {
        dir: uploads.dir.clone(),
        max_file_size: uploads.max_file_size,
        max_field_size: uploads.max_field_size,
        max_files: uploads.max_files,
        max_parts: uploads.max_parts,
        max_part_headers: uploads.max_part_headers,
    };

    // Extensions are compiled in; register the HTTP front (and any others) here, each
    // with its config. With none registered there is nothing to serve, so exit before
    // booting PHP.
    let mut host: ExtensionRuntime = ExtensionRuntime::new();
    host.register::<HttpServer>(http_cfg)?;
    if host.is_empty() {
        return Ok(());
    }

    // Master-side pre-fork binds: every worker inherits these fds; the master
    // holds them for its whole life so respawned generations re-inherit.
    let mut prepare_ctx: PrepareCtx = PrepareCtx::new();
    host.prepare_all(&mut prepare_ctx)?;
    let listeners: Vec<i32> = prepare_ctx.listener_fds().to_vec();

    // Both forms run the same worker model; --classic only changes whether the script is
    // re-included per request (Classic) or stays resident (Worker).
    let mode = if settings.pool.classic {
        Mode::Classic
    } else {
        Mode::Dispatcher(script.clone())
    };

    // PHP MINIT once, in the still-single-threaded master (opcache SHM created
    // here is shared with every forked worker). Workers never tear this down.
    let module: php_sys::PhpModule = Rapira::boot_master()?;

    // Reload needs at most `processes + 1` slots (one overlap headroom worker);
    // 2x is generous slack. Reject configs the board cannot hold instead of
    // silently clamping below the configured worker count.
    anyhow::ensure!(
        settings.pool.processes <= rapira_scoreboard::SB_MAX_SLOTS / 2,
        "pool.processes ({}) exceeds the supported maximum ({})",
        settings.pool.processes,
        rapira_scoreboard::SB_MAX_SLOTS / 2
    );
    let scoreboard: Scoreboard = Scoreboard::create(settings.pool.processes * 2)?;

    let cfg: rapira_master::MasterConfig = rapira_master::MasterConfig {
        processes: settings.pool.processes,
        pool_mode: match settings.pool.mode {
            PoolMode::Static => rapira_master::PoolMode::Static,
            PoolMode::Dynamic {
                min_spare,
                max_spare,
            } => rapira_master::PoolMode::Dynamic {
                min_spare,
                max_spare,
            },
            PoolMode::Ondemand => rapira_master::PoolMode::Ondemand,
        },
        process_idle_timeout: settings.pool.process_idle_timeout,
        process_control_timeout: settings.supervisor.process_control_timeout,
        request_terminate_timeout: settings.pool.request_terminate_timeout,
        pidfile: settings.supervisor.pidfile.clone(),
        listeners,
    };
    let max_requests: u64 = settings.pool.max_requests;

    // The closure runs ONLY in freshly-forked children: each child's COW copy
    // of `host_cell` is Some, taken exactly once per child. The parent's copy
    // stays untouched (and keeps the prepared fds alive for re-inheritance).
    let mut host_cell: Option<ExtensionRuntime> = Some(host);
    let stop: Result<rapira_master::StopReason, anyhow::Error> =
        rapira_master::run(cfg, scoreboard, move |env: rapira_master::WorkerEnv| {
            let host: ExtensionRuntime = host_cell.take().expect("fresh child owns the host copy");
            worker::worker_body(
                env,
                host,
                mode.clone(),
                script.clone(),
                max_requests,
                upload_limits.clone(),
            )
        });

    match stop {
        Ok(rapira_master::StopReason::Drained) => {
            drop(module); // clean php_module_shutdown in the master
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

    /// The sweep must only reclaim dirs whose owning process is gone: foreign
    /// names, malformed pids, and live processes (this one) all stay.
    #[test]
    fn spool_sweep_reclaims_only_dead_pid_dirs() {
        assert!(!spool_dir_reclaimable("other-dir"));
        assert!(!spool_dir_reclaimable("rapira-spool-"));
        assert!(!spool_dir_reclaimable("rapira-spool-x"));
        assert!(!spool_dir_reclaimable("rapira-spool--5"));
        // pid 0 signals the whole process group; never probe it
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
