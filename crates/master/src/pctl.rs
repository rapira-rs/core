//! Process-control state machine: stop escalation and rolling reload. Pure
//! decision logic — no syscalls. The executor (`events.rs`) turns the returned
//! actions into kills and deadlines. Implements state override precedence and
//! QUIT→TERM→KILL escalation, with the owner amendment that the first stop is
//! graceful (QUIT), not TERM.

use libc::c_int;

/// Escalation phase: the signal *already sent*; the next deadline advances it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KillPhase {
    Quit,
    Term,
    Kill,
}

impl KillPhase {
    /// Advance one step and return the signal to send now. Terminal `Kill`
    /// re-sends `SIGKILL` on each subsequent step.
    fn advance(&mut self) -> c_int {
        match self {
            KillPhase::Quit => {
                *self = KillPhase::Term;
                libc::SIGTERM
            }
            KillPhase::Term => {
                *self = KillPhase::Kill;
                libc::SIGKILL
            }
            KillPhase::Kill => libc::SIGKILL,
        }
    }
}

/// Sub-phase of an overlap reload. The reload alternates between waiting for a
/// fresh worker to start accepting and draining one old worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReloadPhase {
    /// Overlap gate: a current-gen replacement was spawned into `slot` (`None`
    /// for ondemand / no headroom) and must reach `SLOT_IDLE` before the next
    /// old worker is drained. No escalation runs here — the executor treats the
    /// pctl deadline as a re-check, capped by `process_control_timeout`.
    Await { slot: Option<usize> },
    /// A QUIT was sent to `draining`; escalate QUIT→TERM→KILL on each deadline.
    Drain {
        draining: libc::pid_t,
        phase: KillPhase,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PctlState {
    Normal,
    /// TERM/INT/QUIT received: drain all, then escalate on each deadline.
    Stopping {
        phase: KillPhase,
    },
    /// USR2/HUP: rolling generation drain that overlaps a fresh worker over each
    /// old one so accept capacity never dips.
    Reloading(ReloadPhase),
}

/// What the executor must do in response to a control byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalAction {
    /// Enter stopping: QUIT every worker, arm the control-timeout deadline.
    Stop,
    /// Second stop signal or TERM while stopping: TERM all and exit forced.
    Forced,
    /// Enter reloading: bump generation and drain the oldest old-gen worker.
    Reload,
    /// Emit a status log line (USR1).
    Status,
    /// No-op (reload/USR2 while already stopping or reloading).
    Ignore,
}

/// Which workers an escalation step targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KillTarget {
    All,
    One(libc::pid_t),
}

/// A single escalation step produced when the pctl deadline fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EscalationStep {
    pub sig: c_int,
    pub target: KillTarget,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Pctl {
    pub state: PctlState,
}

impl Default for Pctl {
    fn default() -> Self {
        Pctl {
            state: PctlState::Normal,
        }
    }
}

impl Pctl {
    pub fn is_normal(&self) -> bool {
        matches!(self.state, PctlState::Normal)
    }

    pub fn is_stopping(&self) -> bool {
        matches!(self.state, PctlState::Stopping { .. })
    }

    /// Map a control byte to an action, applying the state transition. Override
    /// precedence: normal < reloading < stopping; nothing overrides stopping
    /// except a second stop (→ forced).
    pub fn on_signal(&mut self, byte: u8) -> SignalAction {
        use crate::signals::{SIG_HUP, SIG_INT, SIG_QUIT, SIG_TERM, SIG_USR1, SIG_USR2};
        match byte {
            SIG_TERM | SIG_INT | SIG_QUIT => match self.state {
                PctlState::Stopping { .. } => SignalAction::Forced,
                _ => {
                    self.state = PctlState::Stopping {
                        phase: KillPhase::Quit,
                    };
                    SignalAction::Stop
                }
            },
            SIG_USR2 | SIG_HUP => match self.state {
                PctlState::Normal => {
                    // Placeholder; the executor's begin_reload sets the real
                    // Await/Drain sub-state immediately.
                    self.state = PctlState::Reloading(ReloadPhase::Await { slot: None });
                    SignalAction::Reload
                }
                _ => SignalAction::Ignore,
            },
            SIG_USR1 => SignalAction::Status,
            _ => SignalAction::Ignore,
        }
    }

    /// The pctl deadline fired: advance the phase and return the kill to send.
    /// `None` in `Normal` (no escalation pending).
    pub fn escalate(&mut self) -> Option<EscalationStep> {
        match &mut self.state {
            PctlState::Normal => None,
            PctlState::Stopping { phase } => Some(EscalationStep {
                sig: phase.advance(),
                target: KillTarget::All,
            }),
            PctlState::Reloading(ReloadPhase::Drain { draining, phase }) => {
                let pid = *draining;
                Some(EscalationStep {
                    sig: phase.advance(),
                    target: KillTarget::One(pid),
                })
            }
            // No worker is being killed while the gate waits for a replacement.
            PctlState::Reloading(ReloadPhase::Await { .. }) => None,
        }
    }

    /// Enter the overlap gate: wait for the replacement in `slot` (if any) to
    /// become IDLE before the next old worker is drained.
    pub fn set_reload_await(&mut self, slot: Option<usize>) {
        self.state = PctlState::Reloading(ReloadPhase::Await { slot });
    }

    /// Begin draining `pid`: QUIT sent, per-worker escalation phase reset to Quit.
    pub fn set_reload_drain(&mut self, pid: libc::pid_t) {
        self.state = PctlState::Reloading(ReloadPhase::Drain {
            draining: pid,
            phase: KillPhase::Quit,
        });
    }

    /// End a reload: back to normal.
    pub fn finish_reload(&mut self) {
        self.state = PctlState::Normal;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::{SIG_HUP, SIG_INT, SIG_QUIT, SIG_TERM, SIG_USR1, SIG_USR2};

    #[test]
    fn normal_stop_signals_enter_stopping() {
        for b in [SIG_TERM, SIG_INT, SIG_QUIT] {
            let mut p = Pctl::default();
            assert_eq!(p.on_signal(b), SignalAction::Stop);
            assert_eq!(
                p.state,
                PctlState::Stopping {
                    phase: KillPhase::Quit
                }
            );
        }
    }

    #[test]
    fn second_stop_signal_is_forced() {
        let mut p = Pctl::default();
        assert_eq!(p.on_signal(SIG_TERM), SignalAction::Stop);
        assert_eq!(p.on_signal(SIG_TERM), SignalAction::Forced);
        assert_eq!(p.on_signal(SIG_INT), SignalAction::Forced);
        assert_eq!(p.on_signal(SIG_QUIT), SignalAction::Forced);
    }

    #[test]
    fn reload_from_normal_only() {
        for b in [SIG_USR2, SIG_HUP] {
            let mut p = Pctl::default();
            assert_eq!(p.on_signal(b), SignalAction::Reload);
            assert!(matches!(p.state, PctlState::Reloading(_)));
        }
    }

    #[test]
    fn reload_ignored_while_stopping_or_reloading() {
        let mut p = Pctl::default();
        p.on_signal(SIG_USR2);
        // Already reloading: another reload is a no-op.
        assert_eq!(p.on_signal(SIG_USR2), SignalAction::Ignore);
        assert_eq!(p.on_signal(SIG_HUP), SignalAction::Ignore);

        let mut p = Pctl::default();
        p.on_signal(SIG_TERM);
        // Stopping overrides: reload ignored.
        assert_eq!(p.on_signal(SIG_USR2), SignalAction::Ignore);
        assert!(p.is_stopping());
    }

    #[test]
    fn stop_overrides_reload() {
        let mut p = Pctl::default();
        p.on_signal(SIG_USR2);
        assert!(matches!(p.state, PctlState::Reloading(_)));
        assert_eq!(p.on_signal(SIG_TERM), SignalAction::Stop);
        assert!(p.is_stopping());
    }

    #[test]
    fn status_is_stateless() {
        let mut p = Pctl::default();
        assert_eq!(p.on_signal(SIG_USR1), SignalAction::Status);
        assert!(p.is_normal());
        p.on_signal(SIG_TERM);
        assert_eq!(p.on_signal(SIG_USR1), SignalAction::Status);
        assert!(p.is_stopping());
    }

    #[test]
    fn stopping_escalation_phase_progression() {
        let mut p = Pctl::default();
        p.on_signal(SIG_TERM); // phase = Quit (QUIT already sent by executor)
        assert_eq!(
            p.escalate(),
            Some(EscalationStep {
                sig: libc::SIGTERM,
                target: KillTarget::All
            })
        );
        assert_eq!(
            p.escalate(),
            Some(EscalationStep {
                sig: libc::SIGKILL,
                target: KillTarget::All
            })
        );
        // Terminal: re-KILL forever.
        assert_eq!(
            p.escalate(),
            Some(EscalationStep {
                sig: libc::SIGKILL,
                target: KillTarget::All
            })
        );
    }

    #[test]
    fn reloading_escalation_targets_the_draining_pid() {
        let mut p = Pctl::default();
        p.on_signal(SIG_USR2);
        p.set_reload_drain(4242);
        assert_eq!(
            p.escalate(),
            Some(EscalationStep {
                sig: libc::SIGTERM,
                target: KillTarget::One(4242)
            })
        );
        assert_eq!(
            p.escalate(),
            Some(EscalationStep {
                sig: libc::SIGKILL,
                target: KillTarget::One(4242)
            })
        );
    }

    #[test]
    fn normal_has_no_escalation() {
        let mut p = Pctl::default();
        assert_eq!(p.escalate(), None);
    }

    #[test]
    fn set_reload_drain_restarts_at_quit_for_next_target() {
        let mut p = Pctl::default();
        p.on_signal(SIG_USR2);
        p.set_reload_drain(1);
        p.escalate(); // advance to Term for pid 1
        p.set_reload_drain(2);
        assert_eq!(
            p.escalate(),
            Some(EscalationStep {
                sig: libc::SIGTERM,
                target: KillTarget::One(2)
            })
        );
    }

    #[test]
    fn await_gate_produces_no_escalation_then_drain_does() {
        let mut p = Pctl::default();
        p.on_signal(SIG_USR2);
        p.set_reload_await(Some(4));
        assert_eq!(
            p.state,
            PctlState::Reloading(ReloadPhase::Await { slot: Some(4) })
        );
        // While the gate waits, no worker is killed.
        assert_eq!(p.escalate(), None);
        // Draining a picked worker resumes escalation from Quit.
        p.set_reload_drain(7);
        assert_eq!(
            p.state,
            PctlState::Reloading(ReloadPhase::Drain {
                draining: 7,
                phase: KillPhase::Quit
            })
        );
        assert_eq!(
            p.escalate(),
            Some(EscalationStep {
                sig: libc::SIGTERM,
                target: KillTarget::One(7)
            })
        );
    }

    #[test]
    fn finish_reload_returns_to_normal() {
        let mut p = Pctl::default();
        p.on_signal(SIG_USR2);
        p.finish_reload();
        assert!(p.is_normal());
    }
}
