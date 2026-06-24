use anyhow::anyhow;
use tokio::sync::mpsc;

use crate::{
    boot::Rapira,
    types::{Context, Frame, Job, Request},
};

const FRAME_CAP: usize = 16; // before ub_write

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
