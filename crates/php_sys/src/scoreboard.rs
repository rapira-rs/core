use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

thread_local! {
    pub static SB: RefCell<Option<(usize, Arc<Scoreboard>)>> = const { RefCell::new(None) };
}

pub enum Event {
    Handled(bool),
    Recycled,
    Restart,
    Unhealthy,
    Healthy,
}

pub fn sb_set(id: usize, board: Arc<Scoreboard>) {
    SB.with_borrow_mut(|sb: &mut Option<(usize, Arc<Scoreboard>)>| {
        *sb = Some((id, board));
    });
}

pub fn sb_update(event: Event) {
    SB.with_borrow(|sb: &Option<(usize, Arc<Scoreboard>)>| {
        if let Some((id, board)) = sb.as_ref() {
            board.update(*id, event);
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

pub struct Scoreboard {
    // we don't know the size
    pub workers: Box<[WorkerStat]>,
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
    pub fn new(workers: usize) -> Arc<Self> {
        Arc::new(Self {
            workers: (0..workers).map(|_| WorkerStat::default()).collect(),
        })
    }

    fn update(&self, worker: usize, event: Event) {
        let Some(w) = self.workers.get(worker) else {
            return;
        };
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
        let workers: Vec<WorkerStatSnapshot> = self
            .workers
            .iter()
            .enumerate()
            .map(|(id, w)| WorkerStatSnapshot {
                id,
                handled: w.handled.load(Ordering::Relaxed),
                errors: w.errors.load(Ordering::Relaxed),
                recycles: w.recycles.load(Ordering::Relaxed),
                restarts: w.restarts.load(Ordering::Relaxed),
                unhealthy: w.unhealthy.load(Ordering::Relaxed),
            })
            .collect();

        ScoreboardSnapshot {
            handled: workers.iter().map(|w: &WorkerStatSnapshot| w.handled).sum(),
            errors: workers.iter().map(|w: &WorkerStatSnapshot| w.errors).sum(),
            recycles: workers
                .iter()
                .map(|w: &WorkerStatSnapshot| w.recycles)
                .sum(),
            restarts: workers
                .iter()
                .map(|w: &WorkerStatSnapshot| w.restarts)
                .sum(),
            unhealthy: workers.iter().filter(|w| w.unhealthy).count(),
            workers,
        }
    }
}
