//! Ordered, bounded terminal delivery independent of the rendering loop.

use std::sync::Arc;

use sylvops_core::{
    ids::SessionId,
    protocol::{ClientRequest, DaemonResponse, MAX_PTY_CHUNK_SIZE},
};
use sylvops_daemon::client::DaemonClient;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

const INPUT_BUDGET: usize = 256 * 1024;
const INPUT_SLOTS: usize = 256;

struct Input {
    bytes: Vec<u8>,
    _budget: OwnedSemaphorePermit,
}

pub(crate) struct InputPump {
    pub session_id: SessionId,
    sender: mpsc::Sender<Input>,
    budget: Arc<Semaphore>,
    pub failure: oneshot::Receiver<String>,
    task: tokio::task::JoinHandle<()>,
}

impl InputPump {
    pub fn new(client: DaemonClient, session_id: SessionId) -> Self {
        Self::with_transport(session_id, move |bytes| {
            let client = client.clone();
            async move {
                matches!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        client.request(&ClientRequest::SessionInput { session_id, bytes })
                    )
                    .await,
                    Ok(Ok(DaemonResponse::Acknowledged))
                )
            }
        })
    }

    fn with_transport<F, Fut>(session_id: SessionId, mut send: F) -> Self
    where
        F: FnMut(Vec<u8>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = bool> + Send,
    {
        let (sender, mut receiver) = mpsc::channel::<Input>(INPUT_SLOTS);
        let (failed, failure) = oneshot::channel();
        let task = tokio::spawn(async move {
            while let Some(input) = receiver.recv().await {
                // Coalesce already queued keys without adding a batching delay.
                let mut bytes = input.bytes.clone();
                let mut held = vec![input];
                while bytes.len() < MAX_PTY_CHUNK_SIZE {
                    let Ok(next) = receiver.try_recv() else { break };
                    bytes.extend_from_slice(&next.bytes);
                    held.push(next);
                }
                for chunk in bytes.chunks(MAX_PTY_CHUNK_SIZE) {
                    if !send(chunk.to_vec()).await {
                        let _ = failed.send("Terminal input delivery failed; pending input was cancelled. Detach and reattach before typing again.".into());
                        return;
                    }
                }
                drop(held);
            }
        });
        Self {
            session_id,
            sender,
            budget: Arc::new(Semaphore::new(INPUT_BUDGET)),
            failure,
            task,
        }
    }

    pub fn enqueue(&self, bytes: Vec<u8>) -> Result<(), &'static str> {
        if bytes.is_empty() {
            return Ok(());
        }
        let size = u32::try_from(bytes.len()).map_err(|_| "Terminal input is too large.")?;
        let permit = self
            .budget
            .clone()
            .try_acquire_many_owned(size)
            .map_err(|_| "Terminal input queue is full; this input was not sent.")?;
        self.sender
            .try_send(Input {
                bytes,
                _budget: permit,
            })
            .map_err(|_| "Terminal input queue is unavailable; this input was not sent.")
    }
}

impl Drop for InputPump {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn delayed_acknowledgements_preserve_two_hundred_keys_without_blocking_enqueue() {
        let (sent, mut received) = mpsc::channel(256);
        let gate = Arc::new(Semaphore::new(0));
        let worker_gate = gate.clone();
        let pump = InputPump::with_transport(SessionId::new(), move |bytes| {
            let sent = sent.clone();
            let gate = worker_gate.clone();
            async move {
                let _permit = gate.acquire().await.unwrap();
                sent.send(bytes).await.unwrap();
                true
            }
        });
        for byte in 0..200_u8 {
            pump.enqueue(vec![byte]).unwrap();
        }
        // The worker can remain blocked on an acknowledgement while the caller continues.
        tokio::task::yield_now().await;
        assert!(received.try_recv().is_err());
        gate.add_permits(1);
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while output.len() < 200 {
                output.extend(received.recv().await.unwrap());
            }
        })
        .await
        .unwrap();
        assert_eq!(output, (0..200_u8).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn byte_budget_and_failures_are_explicit_and_never_retry_input() {
        let mut pump = InputPump::with_transport(SessionId::new(), |_| async { false });
        assert!(pump.enqueue(vec![0; INPUT_BUDGET + 1]).is_err());
        pump.enqueue(b"first".to_vec()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), &mut pump.failure)
                .await
                .unwrap()
                .is_ok()
        );
        assert!(pump.enqueue(b"later".to_vec()).is_err());
    }
}
