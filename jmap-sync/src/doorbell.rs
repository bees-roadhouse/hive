//! The EventSource doorbell. Push is latency sugar only — correctness comes
//! from state-string polling — so every failure path degrades to the poll
//! cadence instead of erroring the account.

use std::time::Duration;

use futures_util::StreamExt;

use crate::{DoorbellWake, Syncer};

impl Syncer {
    /// Wait for an Email change signal or `timeout`, whichever comes first.
    /// Re-establishes the SSE stream lazily; a server that refuses the
    /// stream costs one debug line per poll interval, nothing more.
    pub async fn wait_doorbell(&mut self, timeout: Duration) -> DoorbellWake {
        if self.doorbell.is_none() {
            match self.raw.doorbell().await {
                Ok(stream) => self.doorbell = Some(stream),
                Err(e) => {
                    tracing::debug!(error = %e, "doorbell unavailable; polling instead");
                    tokio::time::sleep(timeout).await;
                    return DoorbellWake::Disconnected;
                }
            }
        }
        let stream = self.doorbell.as_mut().expect("stream just ensured");
        tokio::select! {
            _ = tokio::time::sleep(timeout) => DoorbellWake::Timeout,
            item = stream.next() => match item {
                Some(Ok(())) => DoorbellWake::Change,
                Some(Err(e)) => {
                    tracing::debug!(error = %e, "doorbell stream errored; will reconnect");
                    self.doorbell = None;
                    DoorbellWake::Disconnected
                }
                None => {
                    self.doorbell = None;
                    DoorbellWake::Disconnected
                }
            },
        }
    }
}
