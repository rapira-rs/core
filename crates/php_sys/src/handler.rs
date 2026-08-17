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

// A buffered response is a Head+Chunk+End trio emitted back-to-back; cap 4
// lets it (plus a stray interim head) queue without parking the PHP thread.
// Longer streams ride the channel's backpressure.
const FRAME_CAP: usize = 4;

/// How long a request may wait for an intake slot before it is shed.
const INTAKE_WAIT: Duration = Duration::from_secs(30);

/// Why a job could not be handed to the pool. Typed so the front can answer
/// 503 for shedding instead of blaming a gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleError {
    /// The intake stayed full for the whole wait budget.
    Saturated,
    /// The pool is gone: draining or stopped.
    Stopped,
}

impl std::fmt::Display for HandleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Saturated => write!(f, "worker pool saturated for {INTAKE_WAIT:?}"),
            Self::Stopped => write!(f, "worker pool stopped"),
        }
    }
}

impl std::error::Error for HandleError {}

#[derive(Clone)]
pub struct RapiraHandle {
    intake: SyncSender<Job>,
    pending: Arc<AtomicUsize>,
    /// The pool serves superglobals ($_SERVER): Context materializes ReqC.
    superglobals: bool,
    /// Exchange-style delivery (`Mode::Dispatcher`).
    dispatcher: bool,
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
            superglobals: self.superglobals,
            dispatcher: self.dispatcher,
        })
    }
}

fn now_unix_f64() -> f64 {
    std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Holds the `pending` increment until the job is accepted: Drop decrements, a
/// successful hand-off disarms. Give-up, disconnect and a panicking blocking
/// task all share the one decrement path.
struct PendingGuard(Option<Arc<AtomicUsize>>);

impl PendingGuard {
    fn arm(pending: &Arc<AtomicUsize>) -> Self {
        pending.fetch_add(1, Ordering::Relaxed);
        Self(Some(pending.clone()))
    }
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(pending) = self.0.take() {
            pending.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl RapiraHandle {
    /// Exchange-style delivery: the host parses multipart before enqueue.
    pub fn dispatcher(&self) -> bool {
        self.dispatcher
    }

    // pending is a diagnostic gauge (Dispatcher::getInfo) - Relaxed. Incremented
    // before the send, decremented on give-up: the consumer decrements as soon
    // as it wakes, so the reverse order could wrap the counter below zero.
    pub async fn handle(&self, mut req: Request) -> Result<mpsc::Receiver<Frame>, HandleError> {
        // a plugin-stamped ingress time wins; this is the fallback for producers
        // that have none (tests preset it to pin the value)
        req.received_at.get_or_insert_with(now_unix_f64);
        let (tx, rx) = mpsc::channel::<Frame>(FRAME_CAP);
        let mut job = Job {
            ctx: Context::new(req, tx, self.superglobals),
        };
        let pending = PendingGuard::arm(&self.pending);
        // Async try_send/sleep instead of a blocking send: the wait cancels with
        // the client (the job then drops undelivered and its Frame sender with
        // it) and parks no blocking-pool thread.
        let deadline = Instant::now() + INTAKE_WAIT;
        loop {
            match self.intake.try_send(job) {
                Ok(()) => {
                    pending.disarm();
                    return Ok(rx);
                }
                Err(TrySendError::Full(j)) => {
                    if Instant::now() > deadline {
                        tracing::warn!(
                            target: "rapira",
                            "intake full for {INTAKE_WAIT:?} ({} pending); shedding the request",
                            self.pending.load(Ordering::Relaxed)
                        );
                        return Err(HandleError::Saturated);
                    }
                    job = j;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(TrySendError::Disconnected(_)) => return Err(HandleError::Stopped),
            }
        }
    }

    pub fn handle_blocking(&self, mut req: Request) -> Result<mpsc::Receiver<Frame>, HandleError> {
        req.received_at.get_or_insert_with(now_unix_f64);
        let (tx, rx) = mpsc::channel::<Frame>(FRAME_CAP);
        let pending = PendingGuard::arm(&self.pending);
        if self
            .intake
            .send(Job {
                ctx: Context::new(req, tx, self.superglobals),
            })
            .is_err()
        {
            return Err(HandleError::Stopped);
        }
        pending.disarm();
        Ok(rx)
    }
}
