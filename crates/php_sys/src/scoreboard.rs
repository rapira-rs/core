//! Worker-side bridge to the shared-memory scoreboard. The slot pointer is
//! thread-local to the single PHP worker thread; every update is a Relaxed
//! atomic on the slot this process owns.

use std::cell::Cell;
use std::sync::atomic::Ordering::Relaxed;

use rapira_scoreboard::{SLOT_ACTIVE, SLOT_DRAINING, SLOT_IDLE, SharedSlot, now_millis};

thread_local! {
    pub static SB: Cell<Option<&'static SharedSlot>> = const { Cell::new(None) };
    // Sticky: once the worker decided to exit (quota/unhealthy), Idle maps to
    // DRAINING so the master stops counting it as spare capacity.
    static DRAINING: Cell<bool> = const { Cell::new(false) };
}

pub enum Event {
    Handled(bool),
    Recycled,
    Restart,
    Unhealthy,
    Healthy,
    Idle,
    Active,
    Draining,
}

pub fn sb_set(slot: &'static SharedSlot) {
    SB.set(Some(slot));
}

pub fn sb_update(event: Event) {
    let Some(s) = SB.get() else { return };
    match event {
        Event::Handled(errored) => {
            s.handled.fetch_add(1, Relaxed);
            if errored {
                s.errors.fetch_add(1, Relaxed);
            }
            // The quota counts fully-finished requests — exactly this event.
            crate::quota::tick();
        }
        Event::Recycled => {
            s.recycles.fetch_add(1, Relaxed);
        }
        Event::Restart => {
            s.restarts.fetch_add(1, Relaxed);
        }
        Event::Unhealthy => {
            s.unhealthy.store(1, Relaxed);
            crate::quota::fire_unhealthy();
        }
        Event::Healthy => s.unhealthy.store(0, Relaxed),
        Event::Idle => {
            let state = if DRAINING.get() {
                SLOT_DRAINING
            } else {
                SLOT_IDLE
            };
            s.state.store(state, Relaxed);
            s.last_activity_ms.store(now_millis(), Relaxed);
        }
        Event::Active => {
            s.state.store(SLOT_ACTIVE, Relaxed);
            s.last_activity_ms.store(now_millis(), Relaxed);
        }
        Event::Draining => {
            DRAINING.set(true);
            // If parked Idle right now, flip immediately; if Active, the next
            // Idle maps to DRAINING via the sticky flag.
            let _ = s
                .state
                .compare_exchange(SLOT_IDLE, SLOT_DRAINING, Relaxed, Relaxed);
        }
    }
}

/// Snapshot types kept for `Rapira::scoreboard()` (in-process tests assert on
/// these fields); filled from the shared board's slots.
#[derive(Debug, Default, Clone)]
pub struct WorkerStatSnapshot {
    pub id: usize,
    pub handled: u64,
    pub errors: u64,
    pub recycles: u64,
    pub restarts: u64,
    pub unhealthy: bool,
}

#[derive(Debug, Default, Clone)]
pub struct ScoreboardSnapshot {
    pub handled: u64,
    pub errors: u64,
    pub recycles: u64,
    pub restarts: u64,
    pub unhealthy: usize, // workers currently flagged unhealthy
    pub workers: Vec<WorkerStatSnapshot>,
}

pub(crate) fn snapshot(board: &rapira_scoreboard::Scoreboard) -> ScoreboardSnapshot {
    let workers: Vec<WorkerStatSnapshot> = board
        .snapshot_slots()
        .into_iter()
        .map(|s| WorkerStatSnapshot {
            id: s.id,
            handled: s.handled,
            errors: s.errors,
            recycles: s.recycles,
            restarts: s.restarts,
            unhealthy: s.unhealthy,
        })
        .collect();
    ScoreboardSnapshot {
        handled: workers.iter().map(|w| w.handled).sum(),
        errors: workers.iter().map(|w| w.errors).sum(),
        recycles: workers.iter().map(|w| w.recycles).sum(),
        restarts: workers.iter().map(|w| w.restarts).sum(),
        unhealthy: workers.iter().filter(|w| w.unhealthy).count(),
        workers,
    }
}
