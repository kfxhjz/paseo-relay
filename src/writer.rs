//! Per-destination writer task.
//!
//! Each attached WebSocket has exactly one writer task owning its write half.
//! All frames and control notifications for that socket go through one bounded
//! channel, which gives per-destination ordering without a global lock. Sources
//! wait for a bounded byte permit and a write acknowledgement, so a slow
//! destination sheds at its deadline instead of growing an unbounded queue.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::io::AsyncWrite;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::capacity::BudgetHold;
use crate::config::FIXED_MESSAGE_OVERHEAD;
use crate::metrics::{Metrics, ShedKind};
use crate::ws::{FrameWriter, OpCode, WsError};

static NEXT_WRITER_ID: AtomicU64 = AtomicU64::new(1);

/// Upper bound for flushing a final WebSocket close frame once cancelled.
const CLOSE_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

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
        deadline: tokio::time::Instant,
        _permit: OwnedSemaphorePermit,
        _budget: Option<Arc<BudgetHold>>,
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

/// When a data frame was last written to a socket.
///
/// The optional idle timeout must not close a socket that is only receiving
/// relayed traffic, so the read side consults this clock as well as its own.
#[derive(Debug)]
pub struct ActivityClock {
    opened: Instant,
    last_write_ms: AtomicU64,
}

impl ActivityClock {
    fn new() -> Self {
        Self {
            opened: Instant::now(),
            last_write_ms: AtomicU64::new(0),
        }
    }

    fn touch(&self) {
        self.last_write_ms
            .store(self.opened.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// Time since the last data frame was written, or since the socket was
    /// opened if none has been.
    pub fn idle_for(&self) -> Duration {
        let last = Duration::from_millis(self.last_write_ms.load(Ordering::Relaxed));
        self.opened.elapsed().saturating_sub(last)
    }
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
    activity: Arc<ActivityClock>,
}

impl WriterHandle {
    pub fn new_id() -> u64 {
        NEXT_WRITER_ID.fetch_add(1, Ordering::Relaxed)
    }

    /// Send one data frame and wait for the destination to finish the write.
    /// The timeout bounds the slow-consumer shed path, and `budget` keeps the
    /// node-wide ingress reservation alive until the writer finishes or drops
    /// the frame (even if the source connection is cancelled first).
    pub async fn send_frame(
        &self,
        opcode: OpCode,
        payload: Bytes,
        timeout: Duration,
        budget: Option<Arc<BudgetHold>>,
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
                deadline,
                _permit: permit,
                _budget: budget,
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
        self.send_frame(OpCode::Text, Bytes::from(payload), timeout, None)
            .await
    }

    pub async fn send_text(&self, payload: Bytes, timeout: Duration) -> Result<(), SendError> {
        self.send_frame(OpCode::Text, payload, timeout, None).await
    }

    pub async fn send_ping(&self, payload: Bytes, timeout: Duration) -> Result<(), SendError> {
        self.send_control(timeout, |ack| Outbound::Ping { payload, ack })
            .await
    }

    pub async fn send_pong(&self, payload: Bytes, timeout: Duration) -> Result<(), SendError> {
        self.send_control(timeout, |ack| Outbound::Pong { payload, ack })
            .await
    }

    /// Queue one control frame and wait for it to be written. Control frames
    /// are at most 125 bytes and take no byte permit; the bounded channel is
    /// their only queue.
    async fn send_control(
        &self,
        timeout: Duration,
        frame: impl FnOnce(oneshot::Sender<Result<(), WsError>>) -> Outbound,
    ) -> Result<(), SendError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + timeout;
        tokio::time::timeout_at(deadline, self.tx.send(frame(ack_tx)))
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

    /// Clock of the last data frame written to this socket.
    pub fn activity(&self) -> Arc<ActivityClock> {
        Arc::clone(&self.activity)
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
    let activity = Arc::new(ActivityClock::new());
    let id = WriterHandle::new_id();

    let handle = WriterHandle {
        id,
        tx,
        permits,
        queue_bytes,
        metrics: Arc::clone(&metrics),
        close_state: Arc::clone(&close_state),
        cancel: cancel.clone(),
        activity: Arc::clone(&activity),
    };

    let task = tokio::spawn(async move {
        let mut writer = FrameWriter::new(write);

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    // Drain queued frames before flushing the close frame so
                    // byte permits and ingress BudgetHolds are released at
                    // once even if the peer's TCP send buffer is full.
                    rx.close();
                    while rx.try_recv().is_ok() {}
                    let close = close_state.lock().expect("close state poisoned").take();
                    if let Some((code, reason)) = close {
                        let close_timeout = write_timeout.min(CLOSE_WRITE_TIMEOUT);
                        let _ = tokio::time::timeout(close_timeout, async {
                            writer.write_close(code, &reason).await?;
                            writer.flush().await
                        })
                        .await;
                    }
                    break;
                }
                message = rx.recv() => {
                    match message {
                        Some(Outbound::Frame {
                            opcode,
                            payload,
                            deadline,
                            _permit,
                            _budget,
                            ack,
                        }) => {
                            // Bound the write by the frame's original enqueue
                            // deadline, not a fresh per-frame write_timeout, so
                            // queued frames whose source already left cannot
                            // keep a slow destination alive for minutes/hours.
                            let remaining =
                                deadline.saturating_duration_since(tokio::time::Instant::now());
                            let (result, timed_out, cancelled_mid_write) = if remaining.is_zero() {
                                (Err(WsError::Closed), true, false)
                            } else {
                                let write_fut = tokio::time::timeout(remaining, async {
                                    writer.write_frame(true, opcode, &payload).await?;
                                    writer.flush().await
                                });
                                tokio::pin!(write_fut);
                                tokio::select! {
                                    biased;
                                    outcome = &mut write_fut => match outcome {
                                        Ok(res) => (res, false, false),
                                        Err(_) => (Err(WsError::Closed), true, false),
                                    },
                                    _ = cancel.cancelled() => {
                                        // Socket was cancelled while stalled mid-write:
                                        // drop queued frames/budgets immediately rather
                                        // than blocking for the remaining delivery window.
                                        (Err(WsError::Closed), false, true)
                                    }
                                }
                            };
                            drop(_permit);
                            drop(_budget);
                            if cancelled_mid_write {
                                let _ = ack.send(result);
                                break;
                            }
                            if result.is_ok() {
                                Metrics::add(&metrics.bytes_forwarded, payload.len() as u64);
                                activity.touch();
                            }
                            let failed = result.is_err();
                            let caller_gone = ack.is_closed();
                            let _ = ack.send(result);
                            if failed {
                                if timed_out && caller_gone {
                                    Metrics::inc(&metrics.slow_consumer_disconnects);
                                    Metrics::inc(&metrics.delivery_timeouts);
                                    metrics.shed(ShedKind::SlowConsumer);
                                }
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
                None,
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
    async fn activity_clock_follows_data_frames_only() {
        let (_client, server) = tokio::io::duplex(64 * 1024);
        let (writer, _task) = spawn_writer(
            server,
            1024 * 1024,
            Duration::from_secs(2),
            Arc::new(Metrics::new()),
        );
        let activity = writer.activity();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(activity.idle_for() >= Duration::from_millis(50));

        // A pong answers the peer's own ping; it is not relayed traffic.
        writer
            .send_pong(Bytes::from_static(b"x"), Duration::from_secs(1))
            .await
            .unwrap();
        assert!(activity.idle_for() >= Duration::from_millis(50));

        writer
            .send_frame(
                OpCode::Text,
                Bytes::from_static(b"hi"),
                Duration::from_secs(1),
                None,
            )
            .await
            .unwrap();
        assert!(activity.idle_for() < Duration::from_millis(50));
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
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SendError::SlowConsumer | SendError::TooLarge));
    }

    #[tokio::test]
    async fn writer_retains_budget_hold_until_write_finishes_or_deadline_drops_queue() {
        use crate::capacity::{Class, ConnIngress, IngressBudget, Reservation};
        let (_client, server) = tokio::io::duplex(1);
        let metrics = Arc::new(Metrics::new());
        let budget = IngressBudget::new(16 * 1024, 0, 4, 0, Duration::ZERO, Arc::clone(&metrics));
        let hold = budget.register();
        let scope = Arc::new(ConnIngress::new(16 * 1024));
        let guard = budget
            .reserve(
                &hold,
                Reservation {
                    weighted: 4096,
                    class: Class::Bulk,
                    frame_need: 4096,
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        let scoped = scope.try_scope(guard).unwrap();
        let msg_budget = BudgetHold::new(vec![scoped]);
        assert_eq!(budget.reserved_bytes(), 4096);

        let (writer, task) = spawn_writer(
            server,
            64 * 1024,
            Duration::from_secs(10),
            Arc::clone(&metrics),
        );

        // Enqueue with a short deadline and immediately cancel the caller future
        // (simulating a source replaced by 1008 while the destination is stalled).
        let send = writer.send_frame(
            OpCode::Binary,
            Bytes::from(vec![0u8; 2048]),
            Duration::from_millis(120),
            Some(Arc::clone(&msg_budget)),
        );
        drop(msg_budget);
        let _ = tokio::time::timeout(Duration::from_millis(20), send).await;

        // Source is gone, yet the frame is still blocked in the destination writer:
        // the budget MUST remain reserved until the writer's deadline expires.
        assert_eq!(budget.reserved_bytes(), 4096);

        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert!(writer.is_closed());
        assert_eq!(budget.reserved_bytes(), 0);
        assert!(metrics.slow_consumer_disconnects.load(Ordering::Relaxed) >= 1);
    }

    #[tokio::test]
    async fn cancel_releases_queued_budget_immediately_even_if_socket_stalls() {
        use crate::capacity::{Class, ConnIngress, IngressBudget, Reservation};
        let (_client, server) = tokio::io::duplex(1);
        let metrics = Arc::new(Metrics::new());
        let budget = IngressBudget::new(16 * 1024, 0, 4, 0, Duration::ZERO, Arc::clone(&metrics));
        let hold = budget.register();
        let scope = Arc::new(ConnIngress::new(16 * 1024));
        let guard = budget
            .reserve(
                &hold,
                Reservation {
                    weighted: 4096,
                    class: Class::Bulk,
                    frame_need: 4096,
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        let msg_budget = BudgetHold::new(vec![scope.try_scope(guard).unwrap()]);

        let (writer, task) = spawn_writer(
            server,
            64 * 1024,
            Duration::from_secs(30),
            Arc::clone(&metrics),
        );
        let send = writer.send_frame(
            OpCode::Binary,
            Bytes::from(vec![0u8; 2048]),
            Duration::from_secs(30),
            Some(Arc::clone(&msg_budget)),
        );
        drop(msg_budget);
        let _ = tokio::time::timeout(Duration::from_millis(20), send).await;
        assert_eq!(budget.reserved_bytes(), 4096);

        // Cancelling the writer must unblock the stalled write and free the
        // held budget immediately rather than waiting 30 s.
        writer.close(1013, "Slow consumer");
        tokio::time::timeout(Duration::from_millis(300), task)
            .await
            .expect("writer task must exit promptly on cancel")
            .unwrap();
        assert_eq!(budget.reserved_bytes(), 0);
    }

    #[tokio::test]
    async fn peer_disconnect_after_source_leaves_does_not_count_as_slow_consumer() {
        use crate::capacity::{Class, ConnIngress, IngressBudget, Reservation};
        let (client, server) = tokio::io::duplex(1);
        let metrics = Arc::new(Metrics::new());
        let budget = IngressBudget::new(16 * 1024, 0, 4, 0, Duration::ZERO, Arc::clone(&metrics));
        let hold = budget.register();
        let scope = Arc::new(ConnIngress::new(16 * 1024));
        let guard = budget
            .reserve(
                &hold,
                Reservation {
                    weighted: 4096,
                    class: Class::Bulk,
                    frame_need: 4096,
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        let msg_budget = BudgetHold::new(vec![scope.try_scope(guard).unwrap()]);

        let (writer, task) = spawn_writer(
            server,
            64 * 1024,
            Duration::from_secs(10),
            Arc::clone(&metrics),
        );
        let send = writer.send_frame(
            OpCode::Binary,
            Bytes::from(vec![0u8; 2048]),
            Duration::from_secs(10),
            Some(Arc::clone(&msg_budget)),
        );
        drop(msg_budget);
        let _ = tokio::time::timeout(Duration::from_millis(20), send).await;

        // Peer disconnects cleanly before the delivery deadline.
        drop(client);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("writer exits on peer EOF")
            .unwrap();
        assert_eq!(budget.reserved_bytes(), 0);
        assert_eq!(metrics.slow_consumer_disconnects.load(Ordering::Relaxed), 0);
    }
}
