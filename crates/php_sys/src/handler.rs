use anyhow::anyhow;
use tokio::sync::mpsc;

use crate::{
    start::Rapira,
    types::{Context, Frame, Job, Request},
};

// `Context::finish` seals the response into exactly one frame, so the channel
// never holds more than one message.
const FRAME_CAP: usize = 1;

/// A cheaply-cloneable handle for submitting jobs to a running [`Rapira`] pool.
///
/// # Shutdown contract
/// Every clone holds a copy of the intake `Sender`. Dropping `Rapira` joins all
/// worker threads after dropping its own `Sender`; the job channel only closes
/// once every `RapiraHandle` clone has also been dropped. Keeping a clone alive
/// past the `Rapira` it came from makes `Drop for Rapira` hang forever.
#[derive(Clone)]
pub struct RapiraHandle {
    intake: mpsc::Sender<Job>,
}

impl Rapira {
    pub fn handle(&self) -> anyhow::Result<RapiraHandle> {
        let intake: &mpsc::Sender<Job> = self
            .intake
            .as_ref()
            .ok_or_else(|| anyhow!("Rapira intake is None"))?;
        Ok(RapiraHandle {
            intake: intake.clone(),
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
}
