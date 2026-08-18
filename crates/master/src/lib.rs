use std::os::fd::{OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::Duration;

use rapira_scoreboard::{Scoreboard, SharedSlot};

mod events;
mod lifeline;
mod pctl;
mod pidfile;
mod process;
mod scaling;
mod signals;

pub use lifeline::{Lifeline, spawn_lifeline_watch};
pub use signals::block_early_signals;

/// Worker exit-code protocol: the worker emits, the master consumes; any other code is a crash.
pub const WORKER_EXIT_DRAINED: i32 = 0;
/// Quota recycle (e.g. max_requests): immediate respawn, no backoff.
pub const WORKER_EXIT_RECYCLE: i32 = 88;
/// Self-reported unhealthy: respawn with backoff; gen-0 with zero handled requests is a boot failure.
pub const WORKER_EXIT_UNHEALTHY: i32 = 89;
/// Exit code the caller uses when [`run`] returns a boot-failure error.
pub const MASTER_EXIT_FAILBOOT: i32 = 70;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scaling {
    Static,
    Dynamic { min_spare: usize, max_spare: usize },
    Ondemand,
}

pub struct MasterConfig {
    /// Static worker count, or the max-children ceiling under dynamic/ondemand.
    pub processes: usize,
    pub scaling: Scaling,
    /// Ondemand only: idle worker lifetime before a QUIT.
    pub process_idle_timeout: Duration,
    /// Stop/reload QUIT to TERM escalation grace.
    pub process_control_timeout: Duration,
    /// Wall-clock bound on one request: the worker is TERM-killed, then KILLed, and replaced. Zero disables.
    pub request_terminate_timeout: Duration,
    pub pidfile: Option<PathBuf>,
    /// Bound listener fds, watched by the poll loop only under `Ondemand`; the master never accepts on them.
    pub listeners: Vec<RawFd>,
}

/// Handed to the worker closure in the child, after post-fork hygiene.
pub struct WorkerEnv {
    /// Read end of the master lifeline: EOF means the master died, so drain.
    pub lifeline: OwnedFd,
    pub slot_view: &'static SharedSlot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// All workers drained cleanly: the caller tears down PHP and exits 0.
    Drained,
    /// Second stop signal or TERM while stopping: the caller exits 130 without PHP teardown.
    Forced,
}

/// Returns in the parent on a clean or forced stop; in a forked child it never returns: the worker closure runs and the child `_exit`s.
pub fn run(
    cfg: MasterConfig,
    scoreboard: Scoreboard,
    worker: impl FnMut(WorkerEnv) -> i32,
) -> anyhow::Result<StopReason> {
    let self_pipe: signals::SelfPipe = signals::install_master_signals()?;
    let lifeline: Lifeline = Lifeline::create()?;
    let _pidfile: Option<pidfile::PidFile> = match &cfg.pidfile {
        Some(p) => Some(pidfile::PidFile::write(p)?),
        None => None,
    };

    let mut master = events::Master::new(cfg, scoreboard, self_pipe, lifeline, worker);
    master.run_loop()
}
