use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

thread_local! {
    pub static SB: RefCell<Option<Arc<Scoreboard>>> = const { RefCell::new(None) };
}

pub enum Event {
    Handled(bool),
    Recycled,
    Restart,
    Unhealthy,
    Healthy,
}

pub fn sb_set(board: Arc<Scoreboard>) {
    SB.with_borrow_mut(|sb: &mut Option<Arc<Scoreboard>>| {
        *sb = Some(board);
    });
}

pub fn sb_update(event: Event) {
    SB.with_borrow(|sb: &Option<Arc<Scoreboard>>| {
        if let Some(board) = sb.as_ref() {
            board.update(event);
        }
    });
}

#[derive(Debug, Default)]
pub struct WorkerStat {
    handled: AtomicU64,
    errors: AtomicU64,
    recycles: AtomicU64,
    restarts: AtomicU64,
    unhealthy: AtomicBool,
}

/// One stat slot: this process runs a single PHP interpreter. The fork-based
/// pool later maps one of these per worker process into shared memory.
#[derive(Debug, Default)]
pub struct Scoreboard {
    worker: WorkerStat,
}

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

impl Scoreboard {
    fn update(&self, event: Event) {
        let w = &self.worker;
        match event {
            Event::Handled(errored) => {
                w.handled.fetch_add(1, Ordering::Relaxed);
                if errored {
                    w.errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            Event::Recycled => {
                w.recycles.fetch_add(1, Ordering::Relaxed);
            }
            Event::Restart => {
                w.restarts.fetch_add(1, Ordering::Relaxed);
            }
            Event::Unhealthy => w.unhealthy.store(true, Ordering::Relaxed),
            Event::Healthy => w.unhealthy.store(false, Ordering::Relaxed),
        }
    }

    pub fn snapshot(&self) -> ScoreboardSnapshot {
        let w = WorkerStatSnapshot {
            id: 0,
            handled: self.worker.handled.load(Ordering::Relaxed),
            errors: self.worker.errors.load(Ordering::Relaxed),
            recycles: self.worker.recycles.load(Ordering::Relaxed),
            restarts: self.worker.restarts.load(Ordering::Relaxed),
            unhealthy: self.worker.unhealthy.load(Ordering::Relaxed),
        };

        ScoreboardSnapshot {
            handled: w.handled,
            errors: w.errors,
            recycles: w.recycles,
            restarts: w.restarts,
            unhealthy: w.unhealthy as usize,
            workers: vec![w],
        }
    }
}
