//! Per-destination writer task.
//!
//! Each attached WebSocket has exactly one writer task owning its write half.
//! All frames and control notifications for that socket go through one bounded
//! channel, which gives per-destination ordering without a global lock. Sources
//! wait for a bounded byte permit and a write acknowledgement, so a slow
//! destination sheds at its deadline instead of growing an unbounded queue.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::AsyncWrite;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::FIXED_MESSAGE_OVERHEAD;
use crate::metrics::Metrics;
use crate::ws::{FrameWriter, OpCode, WsError};

static NEXT_WRITER_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("destination closed")]
    Closed,
    #[error("destination is a slow consumer")]
    SlowConsumer,
    #[error("message does not fit the writer queue")]
    TooLarge,
    #[error("writer error: {0}")]
    Writer(String),
}

enum Outbound {
    Frame {
        opcode: OpCode,
        payload: Bytes,
        _permit: OwnedSemaphorePermit,
        ack: oneshot::Sender<Result<(), WsError>>,
    },
    Ping {
        payload: Bytes,
        ack: oneshot::Sender<Result<(), WsError>>,
    },
    Pong {
        payload: Bytes,
        ack: oneshot::Sender<Result<(), WsError>>,
    },
}

#[derive(Debug, Clone)]
pub struct WriterHandle {
    pub id: u64,
    tx: mpsc::Sender<Outbound>,
    permits: Arc<Semaphore>,
    queue_bytes: usize,
    metrics: Arc<Metrics>,
    close_state: Arc<Mutex<Option<(u16, String)>>>,
    cancel: CancellationToken,
}

impl WriterHandle {
    pub fn new_id() -> u64 {
        NEXT_WRITER_ID.fetch_add(1, Ordering::Relaxed)
    }

    /// Send one data frame and wait for the destination to finish the write.
    /// The timeout bounds the slow-consumer shed path.
    pub async fn send_frame(
        &self,
        opcode: OpCode,
        payload: Bytes,
        timeout: Duration,
    ) -> Result<(), SendError> {
        let weighted = payload
            .len()
            .saturating_add(FIXED_MESSAGE_OVERHEAD as usize);
        if weighted > self.queue_bytes {
            return Err(SendError::TooLarge);
        }
        let weighted = u32::try_from(weighted).map_err(|_| SendError::TooLarge)?;
        // One absolute deadline for the whole delivery (permit + enqueue + ack)
        // so a slow destination cannot multiply the configured timeout.
        let deadline = tokio::time::Instant::now() + timeout;
        let permit = tokio::time::timeout_at(
            deadline,
            Arc::clone(&self.permits).acquire_many_owned(weighted),
        )
        .await
        .map_err(|_| SendError::SlowConsumer)?
        .map_err(|_| SendError::Closed)?;

        let (ack_tx, ack_rx) = oneshot::channel();
        tokio::time::timeout_at(
            deadline,
            self.tx.send(Outbound::Frame {
                opcode,
                payload,
                _permit: permit,
                ack: ack_tx,
            }),
        )
        .await
        .map_err(|_| SendError::SlowConsumer)?
        .map_err(|_| SendError::Closed)?;

        match tokio::time::timeout_at(deadline, ack_rx).await {
            Ok(Ok(Ok(()))) => {
                Metrics::inc(&self.metrics.frames_forwarded);
                Ok(())
            }
            Ok(Ok(Err(error))) => Err(SendError::Writer(error.to_string())),
            Ok(Err(_)) => Err(SendError::Closed),
            Err(_) => Err(SendError::SlowConsumer),
        }
    }

    pub async fn send_json(
        &self,
        value: &serde_json::Value,
        timeout: Duration,
    ) -> Result<(), SendError> {
        let payload = serde_json::to_vec(value).map_err(|_| SendError::TooLarge)?;
        self.send_frame(OpCode::Text, Bytes::from(payload), timeout)
            .await
    }

    pub async fn send_text(&self, payload: Bytes, timeout: Duration) -> Result<(), SendError> {
        self.send_frame(OpCode::Text, payload, timeout).await
    }

    pub async fn send_ping(&self, payload: Bytes, timeout: Duration) -> Result<(), SendError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + timeout;
        tokio::time::timeout_at(
            deadline,
            self.tx.send(Outbound::Ping {
                payload,
                ack: ack_tx,
            }),
        )
        .await
        .map_err(|_| SendError::SlowConsumer)?
        .map_err(|_| SendError::Closed)?;
        match tokio::time::timeout_at(deadline, ack_rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(SendError::Writer(error.to_string())),
            Ok(Err(_)) => Err(SendError::Closed),
            Err(_) => Err(SendError::SlowConsumer),
        }
    }

    pub async fn send_pong(&self, payload: Bytes, timeout: Duration) -> Result<(), SendError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + timeout;
        tokio::time::timeout_at(
            deadline,
            self.tx.send(Outbound::Pong {
                payload,
                ack: ack_tx,
            }),
        )
        .await
        .map_err(|_| SendError::SlowConsumer)?
        .map_err(|_| SendError::Closed)?;
        match tokio::time::timeout_at(deadline, ack_rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(SendError::Writer(error.to_string())),
            Ok(Err(_)) => Err(SendError::Closed),
            Err(_) => Err(SendError::SlowConsumer),
        }
    }

    /// Close the destination. The writer task gets one chance to emit a close
    /// frame; this is used both for protocol closes and slow-consumer shed.
    pub fn close(&self, code: u16, reason: impl Into<String>) {
        {
            let mut state = self.close_state.lock().expect("close state poisoned");
            if state.is_none() {
                *state = Some((code, reason.into()));
            }
        }
        self.cancel.cancel();
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn is_closed(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }
}

/// Start the writer task for one destination. The returned handle is cheap to
/// clone and is stored in the session topology; the task stops when the socket
/// closes or `close` is called.
pub fn spawn_writer<W>(
    write: W,
    queue_bytes: usize,
    write_timeout: Duration,
    metrics: Arc<Metrics>,
) -> (WriterHandle, JoinHandle<()>)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Outbound>(1024);
    let permits = Arc::new(Semaphore::new(queue_bytes));
    let close_state = Arc::new(Mutex::new(None));
    let cancel = CancellationToken::new();
    let id = WriterHandle::new_id();

    let handle = WriterHandle {
        id,
        tx,
        permits,
        queue_bytes,
        metrics: Arc::clone(&metrics),
        close_state: Arc::clone(&close_state),
        cancel: cancel.clone(),
    };

    let task = tokio::spawn(async move {
        let mut writer = FrameWriter::new(write);

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let close = close_state.lock().expect("close state poisoned").take();
                    if let Some((code, reason)) = close {
                        let _ = tokio::time::timeout(write_timeout, async {
                            writer.write_close(code, &reason).await?;
                            writer.flush().await
                        })
                        .await;
                    }
                    break;
                }
                message = rx.recv() => {
                    match message {
                        Some(Outbound::Frame { opcode, payload, _permit, ack }) => {
                            let result = tokio::time::timeout(write_timeout, async {
                                writer.write_frame(true, opcode, &payload).await?;
                                writer.flush().await
                            })
                            .await
                            .unwrap_or(Err(WsError::Closed));
                            drop(_permit);
                            if result.is_ok() {
                                Metrics::add(&metrics.bytes_forwarded, payload.len() as u64);
                            }
                            let failed = result.is_err();
                            let _ = ack.send(result);
                            if failed {
                                break;
                            }
                        }
                        Some(Outbound::Ping { payload, ack }) => {
                            let result = tokio::time::timeout(write_timeout, async {
                                writer.write_ping(&payload).await?;
                                writer.flush().await
                            })
                            .await
                            .unwrap_or(Err(WsError::Closed));
                            let failed = result.is_err();
                            let _ = ack.send(result);
                            if failed {
                                break;
                            }
                        }
                        Some(Outbound::Pong { payload, ack }) => {
                            let result = tokio::time::timeout(write_timeout, async {
                                writer.write_pong(&payload).await?;
                                writer.flush().await
                            })
                            .await
                            .unwrap_or(Err(WsError::Closed));
                            let failed = result.is_err();
                            let _ = ack.send(result);
                            if failed {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        cancel.cancel();
        drop(rx);
    });

    (handle, task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn sends_frames_and_reports_metrics() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let metrics = Arc::new(Metrics::new());
        let (writer, task) =
            spawn_writer(server, 1024 * 1024, Duration::from_secs(2), metrics.clone());
        writer
            .send_frame(
                OpCode::Text,
                Bytes::from_static(b"hello"),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        writer.close(1000, "done");
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert_eq!(metrics.frames_forwarded.load(Ordering::Relaxed), 1);
        let mut buf = [0u8; 11];
        let mut client = client;
        let mut read = 0;
        while read < buf.len() {
            match client.read(&mut buf[read..]).await {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(_) => break,
            }
        }
        assert!(read > 0);
        // First server frame is unmasked text "hello": 0x81 0x05 ...
        assert_eq!(buf[0], 0x81);
        assert_eq!(&buf[2..7], b"hello");
    }

    #[tokio::test]
    async fn slow_consumer_errors() {
        let (_client, server) = tokio::io::duplex(1);
        let metrics = Arc::new(Metrics::new());
        let (writer, _task) = spawn_writer(server, 512, Duration::from_secs(1), metrics);
        let err = writer
            .send_frame(
                OpCode::Binary,
                Bytes::from(vec![0u8; 1024]),
                Duration::from_millis(20),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SendError::SlowConsumer | SendError::TooLarge));
    }
}
