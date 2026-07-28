//! Process-manager scaling decisions: dynamic per-tick spawn/trim and the
//! ondemand arming invariant. Pure logic for idle-server maintenance (spare
//! thresholds, spawn-rate doubling) plus the ondemand suppression state we hold
//! in place of an edge-triggered event backend.

/// Ceiling on the doubling spawn-burst counter; not configurable.
pub(crate) const MAX_SPAWN_RATE: u32 = 32;

/// Snapshot fed to `dynamic_tick`, derived from the scoreboard + proc table.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DynInput {
    pub idle: usize,
    pub running: usize,
    pub min_spare: usize,
    pub max_spare: usize,
    pub max_children: usize,
}

/// Decision for one dynamic tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DynAction {
    /// Too many idle: QUIT the oldest idle worker (KILL if already idle-killing).
    KillOldestIdle,
    /// Too few idle: fork this many, respecting the max_children ceiling.
    Spawn(usize),
    /// Below min_spare but already at max_children: warn, nothing to do.
    ReachedMaxChildren,
    /// Within [min_spare, max_spare]: steady state.
    Steady,
}

/// One dynamic maintenance tick. Mutates `spawn_rate` (the doubling burst
/// counter): reset to 1 on trim/steady/ceiling, double (capped) after a spawn
/// burst.
pub(crate) fn dynamic_tick(inp: &DynInput, spawn_rate: &mut u32) -> DynAction {
    if inp.idle > inp.max_spare {
        *spawn_rate = 1;
        return DynAction::KillOldestIdle;
    }
    if inp.idle < inp.min_spare {
        let want = (inp.min_spare - inp.idle)
            .min(*spawn_rate as usize)
            .min(inp.max_children.saturating_sub(inp.running));
        if want == 0 {
            *spawn_rate = 1;
            return DynAction::ReachedMaxChildren;
        }
        *spawn_rate = spawn_rate.saturating_mul(2).min(MAX_SPAWN_RATE);
        return DynAction::Spawn(want);
    }
    *spawn_rate = 1;
    DynAction::Steady
}

/// Dynamic initial cohort: midpoint of the spare band, clamped to max_children.
/// `min_spare <= max_spare` is enforced when the config is merged, a crate away;
/// saturating here keeps the function total on its own.
pub(crate) fn dynamic_start_count(
    min_spare: usize,
    max_spare: usize,
    max_children: usize,
) -> usize {
    (min_spare + max_spare.saturating_sub(min_spare) / 2).min(max_children)
}

/// Ondemand arming invariant. We watch the listeners in poll only when a fork is
/// worth doing on the next readable event; arming while an idle worker is parked
/// in accept, or a fork is already in flight, would busy-spin level-triggered
/// poll between fork and the child's accept.
pub(crate) fn ondemand_armed(
    is_normal: bool,
    running: usize,
    max_children: usize,
    idle: usize,
    starting: usize,
) -> bool {
    is_normal && running < max_children && idle == 0 && starting == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inp(idle: usize, running: usize) -> DynInput {
        DynInput {
            idle,
            running,
            min_spare: 2,
            max_spare: 4,
            max_children: 8,
        }
    }

    #[test]
    fn too_many_idle_trims_and_resets_rate() {
        let mut rate = 8;
        assert_eq!(
            dynamic_tick(&inp(5, 5), &mut rate),
            DynAction::KillOldestIdle
        );
        assert_eq!(rate, 1);
    }

    #[test]
    fn within_band_is_steady_and_resets_rate() {
        let mut rate = 4;
        assert_eq!(dynamic_tick(&inp(2, 2), &mut rate), DynAction::Steady);
        assert_eq!(rate, 1);
        let mut rate = 4;
        assert_eq!(dynamic_tick(&inp(4, 4), &mut rate), DynAction::Steady);
        assert_eq!(rate, 1);
    }

    #[test]
    fn below_min_spawns_capped_by_deficit_and_rate() {
        // deficit = min_spare(2) - idle(0) = 2; rate = 1 → want = 1.
        let mut rate = 1;
        assert_eq!(dynamic_tick(&inp(0, 1), &mut rate), DynAction::Spawn(1));
        assert_eq!(rate, 2);
        // deficit = 2, rate = 2 → want = 2.
        let mut rate = 2;
        assert_eq!(dynamic_tick(&inp(0, 1), &mut rate), DynAction::Spawn(2));
        assert_eq!(rate, 4);
    }

    #[test]
    fn spawn_rate_doubles_and_caps_at_max() {
        let mut rate = MAX_SPAWN_RATE;
        // large deficit so want is bounded by rate, not deficit
        let big = DynInput {
            idle: 0,
            running: 0,
            min_spare: 100,
            max_spare: 200,
            max_children: 1000,
        };
        assert_eq!(dynamic_tick(&big, &mut rate), DynAction::Spawn(32));
        assert_eq!(rate, MAX_SPAWN_RATE); // capped, not 64
    }

    #[test]
    fn spawn_is_bounded_by_max_children_headroom() {
        // running 7, max_children 8 → headroom 1, even with a big deficit/rate.
        let mut rate = 8;
        let i = DynInput {
            idle: 0,
            running: 7,
            min_spare: 4,
            max_spare: 6,
            max_children: 8,
        };
        assert_eq!(dynamic_tick(&i, &mut rate), DynAction::Spawn(1));
    }

    #[test]
    fn at_max_children_reports_ceiling_and_resets_rate() {
        let mut rate = 8;
        let i = DynInput {
            idle: 0,
            running: 8,
            min_spare: 2,
            max_spare: 4,
            max_children: 8,
        };
        assert_eq!(dynamic_tick(&i, &mut rate), DynAction::ReachedMaxChildren);
        assert_eq!(rate, 1);
    }

    #[test]
    fn start_count_formula() {
        assert_eq!(dynamic_start_count(2, 6, 10), 4); // 2 + (6-2)/2
        assert_eq!(dynamic_start_count(1, 1, 10), 1); // min==max
        assert_eq!(dynamic_start_count(5, 20, 8), 8); // clamped to max_children
    }

    #[test]
    fn ondemand_arming_invariant() {
        // armed only when normal, below ceiling, no idle, no starting.
        assert!(ondemand_armed(true, 0, 4, 0, 0));
        assert!(!ondemand_armed(false, 0, 4, 0, 0)); // stopping/reloading
        assert!(!ondemand_armed(true, 4, 4, 0, 0)); // at ceiling
        assert!(!ondemand_armed(true, 1, 4, 1, 0)); // an idle worker will take it
        assert!(!ondemand_armed(true, 1, 4, 0, 1)); // a fork is already in flight
    }
}
