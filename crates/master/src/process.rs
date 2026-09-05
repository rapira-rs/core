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

pub(crate) const QUICK_CRASH: Duration = Duration::from_secs(10);
pub(crate) const RESPAWN_BASE: Duration = Duration::from_millis(100);

/// Idle trim and the request-timeout watchdog target disjoint worker states, so one field holds whichever kill is under way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KillIntent {
    Idle,
    Timeout,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct WorkerProc {
    pub pid: libc::pid_t,
    pub slot: usize,
    pub generation: u32,
    pub spawned_at: Instant,
    pub kill_intent: Option<KillIntent>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SlotState {
    pub crash_streak: u32,
    pub respawn_at: Option<Instant>,
}

impl SlotState {
    pub fn schedule_backoff(&mut self, lived: Duration, now: Instant) {
        if lived >= QUICK_CRASH {
            self.crash_streak = 0;
        }
        let delay = backoff_delay(self.crash_streak);
        self.crash_streak = self.crash_streak.saturating_add(1);
        self.respawn_at = Some(now + delay);
    }

    pub fn schedule_immediate(&mut self, now: Instant) {
        self.crash_streak = 0;
        self.respawn_at = Some(now);
    }

    pub fn cancel_respawn(&mut self) {
        self.respawn_at = None;
    }
}

pub(crate) fn backoff_delay(streak: u32) -> Duration {
    RESPAWN_BASE.saturating_mul(2u32.saturating_pow(streak.min(8)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitVerdict {
    Drain,
    Recycle,
    Unhealthy,
    IdleKill,
    TimeoutKill,
    Crash,
}

pub(crate) fn classify(status: c_int, intent: Option<KillIntent>) -> ExitVerdict {
    if libc::WIFEXITED(status) {
        match libc::WEXITSTATUS(status) {
            WORKER_EXIT_DRAINED if intent == Some(KillIntent::Idle) => ExitVerdict::IdleKill,
            WORKER_EXIT_DRAINED => ExitVerdict::Drain,
            WORKER_EXIT_RECYCLE => ExitVerdict::Recycle,
            WORKER_EXIT_UNHEALTHY => ExitVerdict::Unhealthy,
            _ => ExitVerdict::Crash,
        }
    } else if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        match intent {
            Some(KillIntent::Idle) if sig == libc::SIGQUIT || sig == libc::SIGKILL => {
                ExitVerdict::IdleKill
            }
            Some(KillIntent::Timeout) if sig == libc::SIGTERM || sig == libc::SIGKILL => {
                ExitVerdict::TimeoutKill
            }
            _ => ExitVerdict::Crash,
        }
    } else {
        ExitVerdict::Crash
    }
}

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

    /// Drains `waitpid` fully so every child ready at this point is buried in one pass.
    pub fn reap_all(&mut self) -> Vec<(WorkerProc, ExitVerdict)> {
        let mut buried = Vec::new();
        loop {
            let mut status: c_int = 0;
            // SAFETY: standard non-blocking reap; status is a live out-param.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            match self.remove(pid) {
                Some(w) => {
                    let verdict = classify(status, w.kill_intent);
                    buried.push((w, verdict));
                }
                None => tracing::warn!(target: "master", "reaped unknown child {pid}"),
            }
        }
        buried
    }
}

/// Fork bracket: the child leaves {QUIT, INT} blocked for the worker's sigwait watcher and `_exit`s under `catch_unwind`, so no master Drop (pidfile unlink, PHP shutdown) can ever run in a child.
pub(crate) fn spawn_worker<F: FnMut(WorkerEnv) -> i32>(
    slot: usize,
    self_pipe: &SelfPipe,
    lifeline: &Lifeline,
    scoreboard: &Scoreboard,
    worker: &mut F,
) -> std::io::Result<libc::pid_t> {
    let lifeline_rd: std::os::fd::OwnedFd = lifeline.rd.try_clone()?;

    let block: libc::sigset_t = sigset(&MASTER_SIGNALS);
    // SAFETY: zeroed sigset_t is fully overwritten by sigprocmask's out-param.
    let mut old: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: block/old are live sigset_ts.
    unsafe { libc::sigprocmask(libc::SIG_BLOCK, &block, &mut old) };

    // SAFETY: fork in a single-threaded master; the child branch is async-signal-safe until _exit.
    match unsafe { libc::fork() } {
        0 => {
            // SAFETY: all calls below are async-signal-safe or operate on fds we own.
            unsafe {
                libc::close(self_pipe.rd.as_raw_fd());
                libc::close(self_pipe.wr.as_raw_fd());
                libc::close(lifeline.wr.as_raw_fd());

                let mut dfl: libc::sigaction = std::mem::zeroed();
                dfl.sa_sigaction = libc::SIG_DFL;
                libc::sigemptyset(&mut dfl.sa_mask);
                for s in MASTER_SIGNALS {
                    libc::sigaction(s, &dfl, std::ptr::null_mut());
                }
                let mut ign: libc::sigaction = std::mem::zeroed();
                ign.sa_sigaction = libc::SIG_IGN;
                libc::sigemptyset(&mut ign.sa_mask);
                libc::sigaction(libc::SIGUSR1, &ign, std::ptr::null_mut());
                libc::sigaction(libc::SIGUSR2, &ign, std::ptr::null_mut());

                #[cfg(target_os = "linux")]
                {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGQUIT);
                    libc::prctl(libc::PR_SET_NAME, c"rapira-worker".as_ptr());
                    if libc::getppid() != master_pid() {
                        libc::kill(libc::getpid(), libc::SIGQUIT);
                    }
                }

                let hold = sigset(&[libc::SIGQUIT, libc::SIGINT]);
                libc::sigprocmask(libc::SIG_SETMASK, &hold, std::ptr::null_mut());
            }

            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let slot_view = scoreboard.slot(slot);
                worker(WorkerEnv {
                    lifeline: lifeline_rd,
                    slot_view,
                })
            }));
            let code = match outcome {
                Ok(code) => code,
                Err(_) => {
                    tracing::error!(target: "master", "worker child panicked; exiting");
                    101
                }
            };
            // SAFETY: async-signal-safe process exit in the child.
            unsafe { libc::_exit(code) }
        }
        -1 => {
            // SAFETY: restore the pre-fork mask.
            unsafe { libc::sigprocmask(libc::SIG_SETMASK, &old, std::ptr::null_mut()) };
            Err(std::io::Error::last_os_error())
        }
        pid => {
            // SAFETY: restore the pre-fork mask.
            unsafe { libc::sigprocmask(libc::SIG_SETMASK, &old, std::ptr::null_mut()) };
            Ok(pid)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Raw wait-status encoding (BSD-derived, shared by Linux and macOS): exit code in bits 8..16, signal in the low 7 bits.
    fn exited(code: i32) -> c_int {
        code << 8
    }
    fn signaled(sig: c_int) -> c_int {
        sig
    }

    #[test]
    fn classify_exit_codes() {
        assert_eq!(classify(exited(0), None), ExitVerdict::Drain);
        assert_eq!(classify(exited(88), None), ExitVerdict::Recycle);
        assert_eq!(classify(exited(89), None), ExitVerdict::Unhealthy);
        assert_eq!(classify(exited(42), None), ExitVerdict::Crash);
        assert_eq!(classify(exited(1), None), ExitVerdict::Crash);
    }

    #[test]
    fn classify_exit_zero_under_kill_intent() {
        assert_eq!(
            classify(exited(0), Some(KillIntent::Idle)),
            ExitVerdict::IdleKill
        );
        assert_eq!(
            classify(exited(0), Some(KillIntent::Timeout)),
            ExitVerdict::Drain
        );
        assert_eq!(
            classify(exited(88), Some(KillIntent::Idle)),
            ExitVerdict::Recycle
        );
        assert_eq!(
            classify(exited(89), Some(KillIntent::Idle)),
            ExitVerdict::Unhealthy
        );
    }

    #[test]
    fn classify_idle_kill_signals() {
        assert_eq!(
            classify(signaled(libc::SIGQUIT), Some(KillIntent::Idle)),
            ExitVerdict::IdleKill
        );
        assert_eq!(
            classify(signaled(libc::SIGKILL), Some(KillIntent::Idle)),
            ExitVerdict::IdleKill
        );
    }

    #[test]
    fn classify_timeout_kill_signals() {
        assert_eq!(
            classify(signaled(libc::SIGTERM), Some(KillIntent::Timeout)),
            ExitVerdict::TimeoutKill
        );
        assert_eq!(
            classify(signaled(libc::SIGKILL), Some(KillIntent::Timeout)),
            ExitVerdict::TimeoutKill
        );
        assert_eq!(
            classify(signaled(libc::SIGQUIT), Some(KillIntent::Timeout)),
            ExitVerdict::Crash
        );
    }

    #[test]
    fn classify_unexpected_signals_are_crashes() {
        assert_eq!(classify(signaled(libc::SIGSEGV), None), ExitVerdict::Crash);
        assert_eq!(classify(signaled(libc::SIGKILL), None), ExitVerdict::Crash);
        assert_eq!(classify(signaled(libc::SIGTERM), None), ExitVerdict::Crash);
        assert_eq!(
            classify(signaled(libc::SIGSEGV), Some(KillIntent::Idle)),
            ExitVerdict::Crash
        );
    }

    #[test]
    fn backoff_progression_and_cap() {
        assert_eq!(backoff_delay(0), Duration::from_millis(100));
        assert_eq!(backoff_delay(1), Duration::from_millis(200));
        assert_eq!(backoff_delay(2), Duration::from_millis(400));
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
        assert_eq!(s.crash_streak, 1);
        assert_eq!(s.respawn_at, Some(now + Duration::from_millis(100)));
    }
}
