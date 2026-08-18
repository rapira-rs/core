use libc::c_int;

/// Escalation phase: the signal already sent; the next deadline advances it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KillPhase {
    Quit,
    Term,
    Kill,
}

impl KillPhase {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReloadPhase {
    /// Gate: the replacement in `slot` must report `SLOT_IDLE` or `SLOT_ACTIVE` before the next old worker is drained; no escalation runs here.
    Await { slot: Option<usize> },
    Drain {
        draining: libc::pid_t,
        phase: KillPhase,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PctlState {
    Normal,
    Stopping { phase: KillPhase },
    Reloading(ReloadPhase),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalAction {
    Stop,
    Forced,
    Reload,
    Status,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KillTarget {
    All,
    One(libc::pid_t),
}

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

    /// Override precedence: normal < reloading < stopping; only TERM/INT overrides stopping (forced), while a retried QUIT stays graceful.
    pub fn on_signal(&mut self, byte: u8) -> SignalAction {
        use crate::signals::{SIG_HUP, SIG_INT, SIG_QUIT, SIG_TERM, SIG_USR1, SIG_USR2};
        match byte {
            SIG_TERM | SIG_INT | SIG_QUIT => match self.state {
                PctlState::Stopping { .. } if byte == SIG_QUIT => SignalAction::Ignore,
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
                    self.state = PctlState::Reloading(ReloadPhase::Await { slot: None });
                    SignalAction::Reload
                }
                _ => SignalAction::Ignore,
            },
            SIG_USR1 => SignalAction::Status,
            _ => SignalAction::Ignore,
        }
    }

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
            PctlState::Reloading(ReloadPhase::Await { .. }) => None,
        }
    }

    pub fn set_reload_await(&mut self, slot: Option<usize>) {
        self.state = PctlState::Reloading(ReloadPhase::Await { slot });
    }

    pub fn set_reload_drain(&mut self, pid: libc::pid_t) {
        self.state = PctlState::Reloading(ReloadPhase::Drain {
            draining: pid,
            phase: KillPhase::Quit,
        });
    }

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
    }

    #[test]
    fn retried_quit_stays_graceful() {
        let mut p = Pctl::default();
        assert_eq!(p.on_signal(SIG_QUIT), SignalAction::Stop);
        assert_eq!(p.on_signal(SIG_QUIT), SignalAction::Ignore);
        assert!(p.is_stopping());
        assert_eq!(p.on_signal(SIG_TERM), SignalAction::Forced);
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
        assert_eq!(p.on_signal(SIG_USR2), SignalAction::Ignore);
        assert_eq!(p.on_signal(SIG_HUP), SignalAction::Ignore);

        let mut p = Pctl::default();
        p.on_signal(SIG_TERM);
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
        p.on_signal(SIG_TERM);
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
        p.escalate();
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
        assert_eq!(p.escalate(), None);
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
