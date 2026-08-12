use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use tokio::sync::mpsc;

use crate::{
    start::Rapira,
    types::{Context, Frame, Job, Request},
};

// `Context::finish` seals the response into exactly one frame, so the channel
// never holds more than one message.
const FRAME_CAP: usize = 1;

#[derive(Clone)]
pub struct RapiraHandle {
    intake: SyncSender<Job>,
    pending: Arc<AtomicUsize>,
}

impl Rapira {
    pub fn handle(&self) -> anyhow::Result<RapiraHandle> {
        let intake = self
            .intake
            .as_ref()
            .ok_or_else(|| anyhow!("Rapira intake is None"))?;
        Ok(RapiraHandle {
            intake: intake.tx.clone(),
            pending: intake.pending.clone(),
        })
    }
}

fn now_unix_f64() -> f64 {
    std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl RapiraHandle {
    // pending is a diagnostic gauge (Dispatcher::getInfo) — Relaxed. Incremented
    // before the send, decremented on give-up: the consumer decrements as soon
    // as it wakes, so the reverse order could wrap the counter below zero.
    pub async fn handle(&self, mut req: Request) -> anyhow::Result<mpsc::Receiver<Frame>> {
        req.received_at = now_unix_f64();
        let (tx, rx) = mpsc::channel::<Frame>(FRAME_CAP);
        let job = Job {
            ctx: Context::new(req, tx),
        };
        self.pending.fetch_add(1, Ordering::Relaxed);
        match self.intake.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(job)) => {
                let tx2 = self.intake.clone();
                let pending = self.pending.clone();
                let sent = tokio::task::spawn_blocking(move || {
                    // Bounded: spawn_blocking tasks cannot be cancelled and the
                    // runtime's Drop waits for them, so an unbounded park could
                    // hang worker shutdown.
                    // https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html
                    let deadline = Instant::now() + Duration::from_secs(30);
                    let mut job = job;
                    loop {
                        match tx2.try_send(job) {
                            Ok(()) => return true,
                            Err(TrySendError::Full(j)) => {
                                if Instant::now() > deadline {
                                    // the closure owns the give-up decrement:
                                    // the awaiting future may already be
                                    // cancelled, this code always runs
                                    pending.fetch_sub(1, Ordering::Relaxed);
                                    return false;
                                }
                                job = j;
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            Err(TrySendError::Disconnected(_)) => {
                                pending.fetch_sub(1, Ordering::Relaxed);
                                return false;
                            }
                        }
                    }
                })
                .await;
                if !matches!(sent, Ok(true)) {
                    return Err(anyhow!("worker pool stopped or saturated"));
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                self.pending.fetch_sub(1, Ordering::Relaxed);
                return Err(anyhow!("worker pool stopped"));
            }
        }
        Ok(rx)
    }

    pub fn handle_blocking(&self, mut req: Request) -> anyhow::Result<mpsc::Receiver<Frame>> {
        req.received_at = now_unix_f64();
        let (tx, rx) = mpsc::channel::<Frame>(FRAME_CAP);
        self.pending.fetch_add(1, Ordering::Relaxed);
        if self
            .intake
            .send(Job {
                ctx: Context::new(req, tx),
            })
            .is_err()
        {
            self.pending.fetch_sub(1, Ordering::Relaxed);
            return Err(anyhow!("worker pool stopped"));
        }
        Ok(rx)
    }
}
