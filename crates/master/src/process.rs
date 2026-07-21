//! Worker process table, the fork bracket, batched reap, exit classification,
//! and per-slot respawn backoff. The fork bracket and `reap_all` are the only
//! syscall-bound pieces; `classify` and the backoff math are pure and unit
//! tested.

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use libc::c_int;

use crate::WorkerEnv;
use crate::lifeline::Lifeline;
#[cfg(target_os = "linux")]
use crate::signals::master_pid;
use crate::signals::{MASTER_SIGNALS, SelfPipe, sigset};
use crate::{WORKER_EXIT_DRAINED, WORKER_EXIT_RECYCLE, WORKER_EXIT_UNHEALTHY};
use rapira_scoreboard::Scoreboard;

/// Quick-crash window: a worker that lived at least this long resets its slot's
/// backoff streak on death.
pub(crate) const QUICK_CRASH: Duration = Duration::from_secs(10);
/// First backoff delay; doubles per consecutive quick crash.
pub(crate) const RESPAWN_BASE: Duration = Duration::from_millis(100);

/// A live worker tracked by the master.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WorkerProc {
    pub pid: libc::pid_t,
    pub slot: usize,
    pub generation: u32,
    pub spawned_at: Instant,
    /// pm idle-kill in progress: QUIT sent; a later pass may KILL.
    pub idle_kill: bool,
}

/// Per-slot respawn bookkeeping (backoff streak + pending deadline).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SlotState {
    pub crash_streak: u32,
    pub respawn_at: Option<Instant>,
}

impl SlotState {
    /// Schedule a backed-off respawn after a crash/unhealthy exit. `lived` is
    /// how long the worker ran; a long-lived worker resets the streak first.
    pub fn schedule_backoff(&mut self, lived: Duration, now: Instant) {
        if lived >= QUICK_CRASH {
            self.crash_streak = 0;
        }
        let delay = backoff_delay(self.crash_streak);
        self.crash_streak = self.crash_streak.saturating_add(1);
        self.respawn_at = Some(now + delay);
    }

    /// Schedule an immediate respawn (clean exit / recycle) and clear the streak.
    pub fn schedule_immediate(&mut self, now: Instant) {
        self.crash_streak = 0;
        self.respawn_at = Some(now);
    }

    pub fn cancel_respawn(&mut self) {
        self.respawn_at = None;
    }
}

/// Backoff delay for a given consecutive-crash streak: 100ms doubling; the
/// exponent saturates at 8, so the delay ceils at 25.6s.
pub(crate) fn backoff_delay(streak: u32) -> Duration {
    RESPAWN_BASE.saturating_mul(2u32.saturating_pow(streak.min(8)))
}

/// Verdict from reaping a worker. Drives respawn policy in the executor.
/// Exits during a stop or reload drain are consumed by their own paths before
/// the verdict is consulted, so no expected-kill tracking is needed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitVerdict {
    /// Exit 0: graceful drain (respawned like a recycle if unexpected).
    Drain,
    /// Recycle exit: immediate respawn, no backoff.
    Recycle,
    /// Unhealthy exit: respawn with backoff (and gen-0 failboot check).
    Unhealthy,
    /// pm idle-kill (QUIT, or KILL after QUIT): trimmed, not respawned.
    IdleKill,
    /// Unknown exit code or unexpected signal: crash (respawn with backoff).
    Crash,
}

/// Classify a raw `wait` status into a verdict. Pure: no syscalls, no state.
/// `idle_kill` is set once the master began an idle-kill on the worker.
pub(crate) fn classify(status: c_int, idle_kill: bool) -> ExitVerdict {
    if libc::WIFEXITED(status) {
        match libc::WEXITSTATUS(status) {
            WORKER_EXIT_DRAINED if idle_kill => ExitVerdict::IdleKill,
            WORKER_EXIT_DRAINED => ExitVerdict::Drain,
            WORKER_EXIT_RECYCLE => ExitVerdict::Recycle,
            WORKER_EXIT_UNHEALTHY => ExitVerdict::Unhealthy,
            _ => ExitVerdict::Crash,
        }
    } else if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        if idle_kill && (sig == libc::SIGQUIT || sig == libc::SIGKILL) {
            ExitVerdict::IdleKill
        } else {
            ExitVerdict::Crash
        }
    } else {
        // Stopped/continued: never requested (no WUNTRACED) → treat as crash.
        ExitVerdict::Crash
    }
}

/// The worker process table: live procs, per-slot backoff, current generation.
pub(crate) struct ProcTable {
    pub procs: Vec<WorkerProc>,
    pub slots: Vec<SlotState>,
    pub generation: u32,
}

impl ProcTable {
    pub fn new(nslots: usize) -> ProcTable {
        ProcTable {
            procs: Vec::new(),
            slots: vec![SlotState::default(); nslots],
            generation: 0,
        }
    }

    pub fn running(&self) -> usize {
        self.procs.len()
    }

    pub fn has_proc(&self, slot: usize) -> bool {
        self.procs.iter().any(|p| p.slot == slot)
    }

    fn remove(&mut self, pid: libc::pid_t) -> Option<WorkerProc> {
        let i = self.procs.iter().position(|p| p.pid == pid)?;
        Some(self.procs.swap_remove(i))
    }

    /// Batched non-blocking reap of every exited child, each paired with its
    /// verdict. Drains `waitpid` fully so every ready child is buried in one pass.
    pub fn reap_all(&mut self) -> Vec<(WorkerProc, ExitVerdict)> {
        let mut buried = Vec::new();
        loop {
            let mut status: c_int = 0;
            // SAFETY: standard non-blocking reap; status is a live out-param.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break; // 0 = none ready, -1 = ECHILD
            }
            match self.remove(pid) {
                Some(w) => {
                    let verdict = classify(status, w.idle_kill);
                    buried.push((w, verdict));
                }
                None => log::warn!(target: "master", "reaped unknown child {pid}"),
            }
        }
        buried
    }
}

/// The fork bracket. Parent blocks the master signal set around `fork`, resets
/// it after; the child neutralizes inherited dispositions, closes the master's
/// control fds, runs the worker closure, and `_exit`s (no Rust drops ever run in
/// a child — no PHP shutdown, no pidfile unlink). Child signal contract:
/// all master dispositions → SIG_DFL, USR1/USR2 → SIG_IGN, {QUIT, INT} left
/// blocked (the worker's sigwait watcher owns them), everything else unblocked
/// including TERM (SIG_DFL fast kill).
pub(crate) fn spawn_worker<F: FnMut(WorkerEnv) -> i32>(
    slot: usize,
    self_pipe: &SelfPipe,
    lifeline: &Lifeline,
    scoreboard: &Scoreboard,
    worker: &mut F,
) -> std::io::Result<libc::pid_t> {
    // Fail in the parent, before the bracket, where the error can be reported;
    // the child inherits its own copy of the dup, the parent's drops on return.
    let lifeline_rd = lifeline.dup_read_end()?;

    // Block the master signal set around fork so no handler runs in the fork
    // window in either process.
    let block = sigset(&MASTER_SIGNALS);
    // SAFETY: zeroed sigset_t is fully overwritten by sigprocmask's out-param.
    let mut old: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: block/old are live sigset_ts.
    unsafe { libc::sigprocmask(libc::SIG_BLOCK, &block, &mut old) };

    // SAFETY: fork in a single-threaded master; the child branch is
    // async-signal-safe until _exit.
    match unsafe { libc::fork() } {
        0 => {
            // ---------------- CHILD ----------------
            // SAFETY: all calls below are async-signal-safe or operate on fds we
            // own; no Rust allocation touches shared locks before hygiene is done.
            unsafe {
                // 1. Drop the master's control plane so it cannot leak/hold EOF.
                libc::close(self_pipe.rd.as_raw_fd());
                libc::close(self_pipe.wr.as_raw_fd());
                libc::close(lifeline.wr.as_raw_fd());

                // 2. Neutralize inherited master dispositions: all → SIG_DFL.
                let mut dfl: libc::sigaction = std::mem::zeroed();
                dfl.sa_sigaction = libc::SIG_DFL;
                libc::sigemptyset(&mut dfl.sa_mask);
                for s in MASTER_SIGNALS {
                    libc::sigaction(s, &dfl, std::ptr::null_mut());
                }
                // USR1/USR2 → SIG_IGN (no reload semantics inside a worker).
                let mut ign: libc::sigaction = std::mem::zeroed();
                ign.sa_sigaction = libc::SIG_IGN;
                libc::sigemptyset(&mut ign.sa_mask);
                libc::sigaction(libc::SIGUSR1, &ign, std::ptr::null_mut());
                libc::sigaction(libc::SIGUSR2, &ign, std::ptr::null_mut());

                #[cfg(target_os = "linux")]
                {
                    // Reliable ONLY because the master is single-threaded:
                    // PDEATHSIG fires when the forking thread exits.
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGQUIT);
                    libc::prctl(libc::PR_SET_NAME, c"rapira-worker".as_ptr());
                    if libc::getppid() != master_pid() {
                        // master died inside the window. Process-directed kill,
                        // not raise: while QUIT is blocked the signal must land
                        // in the shared pending set the worker's sigwait watcher
                        // (another thread) dequeues, not this thread's private
                        // pending set which the watcher never sees.
                        libc::kill(libc::getpid(), libc::SIGQUIT);
                    }
                }

                // 3. Final mask: exactly {QUIT, INT} blocked for the worker's
                //    sigwait watcher (graceful drain); everything else unblocked,
                //    TERM = SIG_DFL fast kill. SETMASK (not unblock-all then
                //    re-block) keeps QUIT blocked throughout, so a QUIT already
                //    pending stays queued for the watcher instead of being
                //    delivered under SIG_DFL during an unblock window.
                let hold = sigset(&[libc::SIGQUIT, libc::SIGINT]);
                libc::sigprocmask(libc::SIG_SETMASK, &hold, std::ptr::null_mut());
            }

            // Enforce "no unwinding past the fork". A panic here (e.g. a
            // thread-spawn `.expect` under EAGAIN) would unwind through the
            // fork point and run the MASTER's Drops in this child — unlinking
            // the shared pidfile, shutting the shared PHP module down against
            // the shared opcache SHM. catch_unwind converts any panic into a
            // plain `_exit`, so no Drop can ever run in a child.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let slot_view = scoreboard
                    .slot(slot)
                    .expect("slot index within scoreboard bounds");
                worker(WorkerEnv {
                    lifeline: lifeline_rd,
                    slot_view,
                })
            }));
            let code = match outcome {
                Ok(code) => code,
                Err(_) => {
                    // Panic already unwound inside the closure; the default hook
                    // printed it. Match Rust's own panic exit code (101).
                    log::error!(target: "master", "worker child panicked; exiting");
                    101
                }
            };
            // No Drop, no PHP module shutdown in children (see above).
            // SAFETY: async-signal-safe process exit in the child.
            unsafe { libc::_exit(code) }
        }
        -1 => {
            // SAFETY: restore the pre-fork mask.
            unsafe { libc::sigprocmask(libc::SIG_SETMASK, &old, std::ptr::null_mut()) };
            Err(std::io::Error::last_os_error())
        }
        pid => {
            // ---------------- PARENT ----------------
            // SAFETY: restore the pre-fork mask.
            unsafe { libc::sigprocmask(libc::SIG_SETMASK, &old, std::ptr::null_mut()) };
            Ok(pid)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Raw wait-status encodings (BSD-derived, shared by Linux and macOS):
    // exited: low 7 bits zero, code in bits 8..16; signaled: signal in low 7.
    fn exited(code: i32) -> c_int {
        code << 8
    }
    fn signaled(sig: c_int) -> c_int {
        sig
    }

    #[test]
    fn classify_exit_codes() {
        assert_eq!(classify(exited(0), false), ExitVerdict::Drain);
        assert_eq!(classify(exited(88), false), ExitVerdict::Recycle);
        assert_eq!(classify(exited(89), false), ExitVerdict::Unhealthy);
        assert_eq!(classify(exited(42), false), ExitVerdict::Crash);
        assert_eq!(classify(exited(1), false), ExitVerdict::Crash);
    }

    #[test]
    fn classify_exit_zero_under_idle_kill_is_idle_kill() {
        // A worker QUIT-drained by an idle-kill exits 0 → IdleKill, not respawn.
        assert_eq!(classify(exited(0), true), ExitVerdict::IdleKill);
        // Recycle/unhealthy codes ignore idle_kill (explicit protocol codes).
        assert_eq!(classify(exited(88), true), ExitVerdict::Recycle);
        assert_eq!(classify(exited(89), true), ExitVerdict::Unhealthy);
    }

    #[test]
    fn classify_idle_kill_signals() {
        // Idle-kill QUIT, and its KILL escalation, are expected deaths.
        assert_eq!(
            classify(signaled(libc::SIGQUIT), true),
            ExitVerdict::IdleKill
        );
        assert_eq!(
            classify(signaled(libc::SIGKILL), true),
            ExitVerdict::IdleKill
        );
    }

    #[test]
    fn classify_unexpected_signals_are_crashes() {
        assert_eq!(classify(signaled(libc::SIGSEGV), false), ExitVerdict::Crash);
        assert_eq!(classify(signaled(libc::SIGKILL), false), ExitVerdict::Crash);
        assert_eq!(classify(signaled(libc::SIGSEGV), true), ExitVerdict::Crash);
    }

    #[test]
    fn backoff_progression_and_cap() {
        assert_eq!(backoff_delay(0), Duration::from_millis(100));
        assert_eq!(backoff_delay(1), Duration::from_millis(200));
        assert_eq!(backoff_delay(2), Duration::from_millis(400));
        // The exponent saturates at 8: higher streaks stay pinned at 25.6s.
        assert_eq!(backoff_delay(8), Duration::from_millis(25_600));
        assert_eq!(backoff_delay(9), Duration::from_millis(25_600));
        assert_eq!(backoff_delay(100), Duration::from_millis(25_600));
    }

    #[test]
    fn schedule_backoff_increments_streak() {
        let now = Instant::now();
        let mut s = SlotState::default();
        s.schedule_backoff(Duration::from_millis(1), now);
        assert_eq!(s.crash_streak, 1);
        assert_eq!(s.respawn_at, Some(now + Duration::from_millis(100)));
        s.schedule_backoff(Duration::from_millis(1), now);
        assert_eq!(s.crash_streak, 2);
        assert_eq!(s.respawn_at, Some(now + Duration::from_millis(200)));
    }

    #[test]
    fn long_lived_crash_resets_streak() {
        let now = Instant::now();
        let mut s = SlotState {
            crash_streak: 5,
            respawn_at: None,
        };
        s.schedule_backoff(QUICK_CRASH, now);
        // Lived past the quick-crash window → streak reset, then bumped to 1.
        assert_eq!(s.crash_streak, 1);
        assert_eq!(s.respawn_at, Some(now + Duration::from_millis(100)));
    }
}
