//! `rapira_master`: a single-threaded, pre-fork process master built on
//! `libc` + `std` + the shared-memory scoreboard.
//!
//! The master boots PHP once (caller side), then forks workers that inherit the
//! warm image. It supervises them with configurable pool modes (static /
//! dynamic / ondemand), graceful stop escalation, and rolling reload. Children
//! never re-enter master code: the fork bracket runs the worker closure and
//! `_exit`s, so no Rust drop (pidfile unlink, PHP shutdown) can ever fire in a
//! worker.
//!
//! Decision/execution split: `pctl`, `scaling`, and the `classify`/backoff logic
//! in `process` are pure and unit tested; `events` is the thin syscall executor.

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

/// Worker exit-code protocol (worker emits, master consumes; unknown = crash).
///
/// Graceful drain: QUIT drain or intake closed.
pub const WORKER_EXIT_DRAINED: i32 = 0;
/// Quota recycle (e.g. max_requests): immediate respawn, no backoff.
pub const WORKER_EXIT_RECYCLE: i32 = 88;
/// Self-reported unhealthy: respawn with backoff (gen-0 with zero handled
/// requests is a boot failure — [`run`] returns an error).
pub const WORKER_EXIT_UNHEALTHY: i32 = 89;
/// Master exit code the caller should use when [`run`] returns a boot-failure
/// error (gen-0 worker died unhealthy before handling any request).
pub const MASTER_EXIT_FAILBOOT: i32 = 70;

/// Pool mode. `processes` in [`MasterConfig`] is the static count for
/// `Static`, and the max-children ceiling for `Dynamic`/`Ondemand`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolMode {
    Static,
    Dynamic { min_spare: usize, max_spare: usize },
    Ondemand,
}

/// Master configuration. `pidfile` and the max_requests-style quota are the
/// only worker-adjacent knobs the master does not itself enforce (the worker
/// closure owns request accounting).
pub struct MasterConfig {
    /// Static worker count / dynamic-ondemand max_children.
    pub processes: usize,
    pub pool_mode: PoolMode,
    /// Ondemand: idle worker lifetime before a QUIT.
    pub process_idle_timeout: Duration,
    /// Stop/reload QUIT→TERM escalation grace.
    pub process_control_timeout: Duration,
    /// Wall-clock bound on a single request: an ACTIVE worker past it is
    /// TERM-killed (then KILLed) and replaced immediately. Zero = disabled.
    pub request_terminate_timeout: Duration,
    pub pidfile: Option<PathBuf>,
    /// Bound listener fds; watched by the poll loop only under `Ondemand` (the
    /// master never accepts on them).
    pub listeners: Vec<RawFd>,
}

/// Handed to the worker closure IN THE CHILD, after post-fork hygiene.
pub struct WorkerEnv {
    /// Read end of the master lifeline: EOF ⇒ master died ⇒ drain.
    pub lifeline: OwnedFd,
    /// This worker's scoreboard slot (shared mmap).
    pub slot_view: &'static SharedSlot,
}

/// Why the master loop returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// All workers drained cleanly (caller tears down PHP, exits 0).
    Drained,
    /// Second stop signal / TERM while stopping (caller exits 130 without PHP
    /// teardown).
    Forced,
}

/// Run the master loop. In the parent this returns only on a clean or forced
/// stop. In each forked child it never returns: after post-fork hygiene the
/// worker closure runs and the child `_exit`s with its return code.
///
/// `worker` is `FnMut` because the parent calls it once per fork; each child
/// observes exactly one call on its own copy-on-write memory.
pub fn run(
    cfg: MasterConfig,
    scoreboard: Scoreboard,
    worker: impl FnMut(WorkerEnv) -> i32,
) -> anyhow::Result<StopReason> {
    // Handlers exist only from here (after MINIT); the fork bracket blocks
    // around each fork so no handler runs in a child window.
    let self_pipe: signals::SelfPipe = signals::install_master_signals()?;
    let lifeline: Lifeline = Lifeline::create()?;
    // Held to end of scope: Drop unlinks the pidfile on every exit path.
    let _pidfile: Option<pidfile::PidFile> = match &cfg.pidfile {
        Some(p) => Some(pidfile::PidFile::write(p)?),
        None => None,
    };

    let mut master = events::Master::new(cfg, scoreboard, self_pipe, lifeline, worker);
    master.run_loop()
}
