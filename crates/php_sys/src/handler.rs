use anyhow::anyhow;
use tokio::sync::mpsc;

use crate::{
    handle_config::ConfigCell,
    start::Rapira,
    types::{Context, Frame, Job, Request},
};

// `Context::finish` seals the response into exactly one frame, so the channel
// never holds more than one message.
const FRAME_CAP: usize = 1;

/// A cheaply-cloneable handle for submitting jobs to a running [`Rapira`] instance.
///
/// # Shutdown contract
/// Every clone holds a copy of the intake `Sender`. Dropping `Rapira` joins the
/// worker thread after dropping its own `Sender`; the job channel only closes
/// once every `RapiraHandle` clone has also been dropped. A clone kept alive past
/// its `Rapira` leaves the worker parked on the open channel — `Drop for Rapira`
/// then gives up after a bounded grace and skips the PHP teardown. Drop all
/// handles first for a clean shutdown.
#[derive(Clone)]
pub struct RapiraHandle {
    intake: mpsc::Sender<Job>,
    /// The blob the worker script declared for its handler (see [`Self::handler_config`]).
    config: ConfigCell,
}

impl Rapira {
    pub fn handle(&self) -> anyhow::Result<RapiraHandle> {
        let intake: &mpsc::Sender<Job> = self
            .intake
            .as_ref()
            .ok_or_else(|| anyhow!("Rapira intake is None"))?;
        Ok(RapiraHandle {
            intake: intake.clone(),
            config: self.config.clone(),
        })
    }
}

impl RapiraHandle {
    /// Submit `req`; the sealed response arrives as a single [`Frame`] (a
    /// channel that closes without one means the worker died).
    pub async fn handle(&self, req: Request) -> anyhow::Result<mpsc::Receiver<Frame>> {
        let (tx, rx) = mpsc::channel::<Frame>(FRAME_CAP);
        self.intake
            .send(Job {
                ctx: Context::new(req, tx),
            })
            .await
            .map_err(|_| anyhow!("worker pool stopped"))?;
        Ok(rx)
    }

    pub fn handle_blocking(&self, req: Request) -> anyhow::Result<mpsc::Receiver<Frame>> {
        let (tx, rx) = mpsc::channel::<Frame>(FRAME_CAP);
        self.intake
            .blocking_send(Job {
                ctx: Context::new(req, tx),
            })
            .map_err(|_| anyhow!("worker pool stopped"))?;
        Ok(rx)
    }

    /// A clone of the opaque config blob the worker script declared for its
    /// handler, or `None` if it hasn't (yet). A lock plus a `Vec` clone of a few
    /// dozen bytes; the caller (an extension) owns the interpretation.
    pub fn handler_config(&self) -> Option<Vec<u8>> {
        self.config.lock().expect("config cell poisoned").clone()
    }
}
