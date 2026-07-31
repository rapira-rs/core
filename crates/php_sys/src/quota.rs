//! Worker request quota (max_requests recycle) and unhealthy-exit hooks.
//! Counting happens on the PHP worker thread at the sites that already report
//! `Event::Handled`; the hooks only announce — draining is the host's job.

use std::cell::RefCell;

use tracing::info;

use crate::scoreboard::{Event, sb_update};

/// Per-worker lifecycle hooks, installed on the PHP worker thread.
#[derive(Default)]
pub struct WorkerHooks {
    /// Effective request quota (jitter already applied by the caller); 0 = unlimited.
    pub max_requests: u64,
    /// Fired exactly once, on the PHP worker thread, when the quota is reached.
    pub on_quota: Option<Box<dyn FnOnce() + Send>>,
    /// Fired exactly once when the worker hits the consecutive-failboot limit.
    pub on_unhealthy: Option<Box<dyn FnOnce() + Send>>,
    /// Scoreboard slot to report into; None -> a private single-slot board.
    pub slot: Option<&'static rapira_scoreboard::SharedSlot>,
}

#[derive(Default)]
struct QuotaState {
    served: u64,
    max: u64,
    on_quota: Option<Box<dyn FnOnce() + Send>>,
    on_unhealthy: Option<Box<dyn FnOnce() + Send>>,
}

thread_local! {
    static Q: RefCell<QuotaState> = RefCell::new(QuotaState::default());
}

/// Install on the PHP worker thread before the first job.
pub(crate) fn install(
    max_requests: u64,
    on_quota: Option<Box<dyn FnOnce() + Send>>,
    on_unhealthy: Option<Box<dyn FnOnce() + Send>>,
) {
    Q.with_borrow_mut(|q| {
        *q = QuotaState {
            served: 0,
            max: max_requests,
            on_quota,
            on_unhealthy,
        };
    });
}

/// One fully-finished request; called from `sb_update(Event::Handled(_))`.
pub(crate) fn tick() {
    Q.with_borrow_mut(|q| {
        if q.max == 0 {
            return;
        }
        q.served += 1;
        if q.served == q.max
            && let Some(f) = q.on_quota.take()
        {
            info!(target: "rapira", "worker served {} requests; recycling", q.served);
            sb_update(Event::Draining);
            f();
        }
    });
}

/// The worker declared itself unable to boot PHP (consecutive failboots).
pub(crate) fn fire_unhealthy() {
    Q.with_borrow_mut(|q| {
        if let Some(f) = q.on_unhealthy.take() {
            sb_update(Event::Draining);
            f();
        }
    });
}
