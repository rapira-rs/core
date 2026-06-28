use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

thread_local! {
    pub static SB: RefCell<Option<(usize, Arc<Scoreboard>)>> = const { RefCell::new(None) };
}

pub fn sb_set(id: usize, board: Arc<Scoreboard>) {
    SB.with_borrow_mut(|sb: &mut Option<(usize, Arc<Scoreboard>)>| {
        *sb = Some((id, board));
    });
}

pub fn sb_record(errored: bool) {
    SB.with_borrow(|sb: &Option<(usize, Arc<Scoreboard>)>| {
        if let Some((id, board)) = sb.as_ref() {
            board.record(*id, errored);
        }
    });
}

#[derive(Debug, Default)]
pub struct WorkerStat {
    handled: AtomicU64,
    errors: AtomicU64,
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
}

#[derive(Debug, Default, Clone)]
pub struct ScoreboardSnapshot {
    pub handled: u64,
    pub errors: u64,
    pub workers: Vec<WorkerStatSnapshot>,
}

impl Scoreboard {
    pub fn new(workers: usize) -> Arc<Self> {
        Arc::new(Self {
            workers: (0..workers).map(|_| WorkerStat::default()).collect(),
        })
    }

    fn record(&self, worker: usize, errored: bool) {
        let Some(w) = self.workers.get(worker) else {
            return;
        };
        w.handled.fetch_add(1, Ordering::Relaxed);
        if errored {
            w.errors.fetch_add(1, Ordering::Relaxed);
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
            })
            .collect();

        ScoreboardSnapshot {
            handled: workers.iter().map(|w: &WorkerStatSnapshot| w.handled).sum(),
            errors: workers.iter().map(|w: &WorkerStatSnapshot| w.errors).sum(),
            workers,
        }
    }
}
