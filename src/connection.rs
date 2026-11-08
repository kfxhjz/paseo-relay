//! Per-WebSocket connection runner.
//!
//! A connection has one read task (frame parsing + ingress reservation) and one
//! writer task (bounded queue). The main task owns session attachment, routing,
//! handshake validation, control notifications, and cleanup. Closing one
//! connection never closes another socket except for the protocol-mandated
//! replacement/detach rules.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::io::AsyncRead;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::capacity::{BudgetError, BudgetGuard, IpGuard, TcpSlot, WsSlot};
use crate::config::{CONTROL_FRAME_BYTES, INGRESS_CHUNK_BYTES};
use crate::handshake::{self, HandshakeOutcome};
use crate::metrics::Metrics;
use crate::protocol::{Connection, Role};
use crate::server::AppState;
use crate::session::Session;
use crate::writer::{spawn_writer, SendError, WriterHandle};
use crate::ws::{self, FrameReader, OpCode, WsError, WsLimits, MAX_FRAGMENTS_PER_MESSAGE};

/// Bounded read-ahead: at most two completed messages/events are handed to the
/// main task; the reader task blocks on the channel once that is full.
const READ_CHANNEL_CAPACITY: usize = 2;

/// Attach/finish notifications must never hold a connection teardown for the
/// full delivery deadline (30 s). Bound them so a saturated control writer
/// cannot delay shutdown close frames.
const CONTROL_SEND_TIMEOUT: Duration = Duration::from_secs(1);

/// Per-connection in-flight ingress budget.
///
/// Guards share this counter; the relay reserves at most
/// [`Config::per_connection_ingress_bytes`] weighted bytes for one connection so
/// a single peer cannot monopolise the node-wide budget through queued
/// messages.
struct ConnIngress {
    used: AtomicU64,
    limit: u64,
}

/// Ingress guard that also releases the connection-scoped counter.
struct ScopedGuard {
    guard: Option<BudgetGuard>,
    scope: Arc<ConnIngress>,
    weighted: u64,
}

impl Drop for ScopedGuard {
    fn drop(&mut self) {
        self.scope.used.fetch_sub(self.weighted, Ordering::Relaxed);
        drop(self.guard.take());
    }
}

enum Inbound {
    Message {
        opcode: OpCode,
        payload: Bytes,
        guards: Vec<ScopedGuard>,
    },
    Ping(Bytes),
    Pong,
    Close {
        code: u16,
        reason: String,
    },
    Failed(Failure),
}

enum Failure {
    Ws(WsError),
    Budget(BudgetError),
    ReadTimeout,
}

impl Failure {
    fn close(&self) -> (u16, String) {
        match self {
            Failure::Ws(error) => (error.close_code(), error.to_string()),
            Failure::Budget(BudgetError::TooLarge) => (1013, "Relay ingress capacity".to_owned()),
            Failure::Budget(BudgetError::Exhausted) => (1013, "Relay ingress capacity".to_owned()),
            Failure::Budget(BudgetError::Closed) => (1013, "Relay ingress capacity".to_owned()),
            Failure::ReadTimeout => (1013, "Relay read timeout".to_owned()),
        }
    }
}

pub async fn run_connection(
    upgraded: Upgraded,
    connection: Connection,
    state: Arc<AppState>,
    ws_slot: WsSlot,
    ip_guard: IpGuard,
    // Held for the whole socket lifetime so MAX_TCP_CONNECTIONS bounds every
    // open TCP socket, including upgraded WebSockets.
    _tcp_slot: Option<TcpSlot>,
    peer: std::net::SocketAddr,
) {
    let io = TokioIo::new(upgraded);
    let (read_half, write_half) = tokio::io::split(io);

    let limits = WsLimits {
        max_frame_payload: if connection.is_v2_control() {
            CONTROL_FRAME_BYTES
        } else {
            state.config.max_message_bytes
        },
        max_message_payload: if connection.is_v2_control() {
            CONTROL_FRAME_BYTES
        } else {
            state.config.max_message_bytes
        },
        max_fragments: MAX_FRAGMENTS_PER_MESSAGE,
    };

    let one_max_message_weighted = state.config.max_message_weighted_charge();
    // The read-ahead pipeline can hold up to four messages per connection
    // (one being handled by the main task, two queued, one being read), so the
    // per-connection cap must not be lower than that or a legitimate pipelining
    // source would be closed. It still prevents one connection from taking more
    // than four maximum messages when the global budget is larger.
    let per_connection_limit = state
        .config
        .ingress_budget_bytes
        .min(one_max_message_weighted.saturating_mul(4))
        .max(one_max_message_weighted);
    let ingress_scope = Arc::new(ConnIngress {
        used: AtomicU64::new(0),
        limit: per_connection_limit,
    });

    let (writer, writer_task) = spawn_writer(
        write_half,
        state.config.writer_queue_bytes(),
        state.config.delivery_timeout,
        Arc::clone(&state.metrics),
    );
    let socket_id = writer.id;

    let (tx, mut rx) = mpsc::channel(READ_CHANNEL_CAPACITY);
    let reader_task = spawn_reader(
        read_half,
        limits,
        tx,
        Arc::clone(&state.capacity),
        Arc::clone(&state.metrics),
        state.config.ingress_wait_timeout,
        state.config.frame_read_timeout,
        state.config.message_read_timeout,
        Arc::clone(&ingress_scope),
    );

    let (session, attach) = match state.sessions.join(&connection, writer.clone(), socket_id) {
        Ok(value) => value,
        Err(error) => {
            Metrics::inc(&state.metrics.session_rejections);
            let reason = match error {
                crate::session::JoinError::SessionLimit => "Relay session capacity",
                crate::session::JoinError::ClientLimit => "Relay client capacity",
            };
            writer.close(1013, reason);
            reader_task.abort();
            let _ = tokio::time::timeout(Duration::from_secs(2), writer_task).await;
            drop(ws_slot);
            drop(ip_guard);
            return;
        }
    };

    for replaced in attach.replaced {
        replaced.close(1008, "Replaced by new connection");
    }
    if let Some(ids) = attach.sync {
        let sync = json!({"type": "sync", "connectionIds": ids});
        let _ = tokio::time::timeout(
            CONTROL_SEND_TIMEOUT,
            writer.send_json(&sync, state.config.delivery_timeout),
        )
        .await;
    }
    if let Some(control) = attach.control {
        let connected = json!({
            "type": "connected",
            "connectionId": connection.connection_id().unwrap_or_default(),
        });
        let sent = tokio::time::timeout(
            CONTROL_SEND_TIMEOUT,
            control
                .writer
                .send_json(&connected, state.config.delivery_timeout),
        )
        .await;
        if !matches!(sent, Ok(Ok(()))) {
            Metrics::inc(&state.metrics.slow_consumer_disconnects);
            control.writer.close(1013, "Slow consumer");
        }
    }

    let control_watchdog = if connection.is_v2_client() {
        let connection_id = connection.connection_id().unwrap_or_default().to_owned();
        if session.data_socket(&connection_id).is_none() {
            Some(spawn_control_watchdog(
                Arc::clone(&state),
                Arc::clone(&session),
                connection_id,
            ))
        } else {
            None
        }
    } else {
        None
    };

    tracing::debug!(
        role = %connection.role,
        version = %connection.version,
        peer = %peer,
        "websocket attached"
    );

    let writer_cancel = writer.cancel_token();
    let close_code: u16;
    let close_reason: String;

    loop {
        tokio::select! {
            biased;
            _ = writer_cancel.cancelled() => {
                close_code = 1013;
                close_reason = "Delivery unavailable".to_owned();
                break;
            }
            _ = state.conn_shutdown.cancelled() => {
                close_code = 1012;
                close_reason = "Relay shutting down".to_owned();
                break;
            }
            event = rx.recv() => {
                let Some(event) = event else {
                    close_code = 1000;
                    close_reason = String::new();
                    break;
                };
                match event {
                    Inbound::Message { opcode, payload, guards } => {
                        let outcome = tokio::select! {
                            biased;
                            _ = state.conn_shutdown.cancelled() => {
                                Err((1012, "Relay shutting down"))
                            }
                            _ = writer_cancel.cancelled() => {
                                Err((1013, "Delivery unavailable"))
                            }
                            result = handle_message(&state, &session, &connection, opcode, &payload) => result,
                        };
                        drop(guards);
                        if let Err((code, reason)) = outcome {
                            close_code = code;
                            close_reason = reason.to_owned();
                            break;
                        }
                    }
                    Inbound::Ping(payload) => {
                        let pong = tokio::select! {
                            biased;
                            _ = state.conn_shutdown.cancelled() => {
                                Err((1012, "Relay shutting down"))
                            }
                            _ = writer_cancel.cancelled() => {
                                Err((1013, "Delivery unavailable"))
                            }
                            result = writer.send_pong(payload, state.config.delivery_timeout) => {
                                result.map_err(|_| (1013, "Delivery unavailable"))
                            }
                        };
                        if let Err((code, reason)) = pong {
                            close_code = code;
                            close_reason = reason.to_owned();
                            break;
                        }
                    }
                    Inbound::Pong => {}
                    Inbound::Close { code, reason } => {
                        close_code = echo_close_code(code);
                        close_reason = reason;
                        break;
                    }
                    Inbound::Failed(failure) => {
                        let (code, reason) = failure.close();
                        close_code = code;
                        close_reason = reason;
                        break;
                    }
                }
            }
        }
    }

    if let Some(watchdog) = control_watchdog {
        watchdog.abort();
    }

    finish(
        &state,
        &connection,
        socket_id,
        &session,
        close_code,
        close_reason,
        &writer,
        reader_task,
        writer_task,
    )
    .await;

    drop(ws_slot);
    drop(ip_guard);
}

#[allow(clippy::too_many_arguments)]
async fn finish(
    state: &Arc<AppState>,
    connection: &Connection,
    socket_id: u64,
    session: &Arc<Session>,
    close_code: u16,
    close_reason: String,
    writer: &WriterHandle,
    reader_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
) {
    let detach = state.sessions.leave(connection, socket_id);
    for close in detach.closes {
        close.socket.close(close.code, close.reason);
    }
    if let Some(notification) = detach.control_notification {
        if let Some(control) = session.control() {
            tokio::select! {
                biased;
                _ = state.conn_shutdown.cancelled() => {}
                result = tokio::time::timeout(
                    CONTROL_SEND_TIMEOUT,
                    control
                        .writer
                        .send_json(&notification, state.config.delivery_timeout),
                ) => {
                    let _ = result;
                }
            }
        }
    }

    writer.close(close_code, close_reason);
    reader_task.abort();
    let mut writer_task = writer_task;
    if tokio::time::timeout(Duration::from_secs(2), &mut writer_task)
        .await
        .is_err()
    {
        writer_task.abort();
    }
    session.notify_data_ready();
}

async fn handle_message(
    state: &Arc<AppState>,
    session: &Arc<Session>,
    connection: &Connection,
    opcode: OpCode,
    payload: &Bytes,
) -> Result<(), (u16, &'static str)> {
    if session.is_empty() {
        return Ok(());
    }

    if connection.is_v2_control() {
        handle_control_input(state, opcode, &session.control(), payload).await;
        return Ok(());
    }

    if connection.role == Role::Client && handshake::looks_like_handshake(payload) {
        match handshake::check(opcode, payload) {
            HandshakeOutcome::Reject(_) => {
                Metrics::inc(&state.metrics.handshake_rejected);
                return Err((1008, "Invalid handshake key"));
            }
            HandshakeOutcome::Accept(_) => {
                Metrics::inc(&state.metrics.handshake_accepted);
            }
            HandshakeOutcome::NotHandshake => {}
        }
    }

    match connection.version {
        crate::protocol::Version::V1 => {
            let peer = session.v1_peer(connection.role);
            if let Some(peer) = peer {
                deliver_one(state, &peer, opcode, payload)
                    .await
                    .map_err(|reason| (1013, reason))?;
            }
        }
        crate::protocol::Version::V2 => {
            if connection.role == Role::Client {
                let Some(connection_id) = connection.connection_id() else {
                    return Ok(());
                };
                let destination = match session.data_socket(connection_id) {
                    Some(socket) => Some(socket),
                    None => {
                        session
                            .wait_for_data(connection_id, state.config.data_attach_timeout)
                            .await
                    }
                };
                let Some(destination) = destination else {
                    return Err((1013, "Data route unavailable"));
                };
                deliver_one(state, &destination, opcode, payload)
                    .await
                    .map_err(|reason| (1013, reason))?;
            } else {
                let Some(connection_id) = connection.connection_id() else {
                    return Ok(());
                };
                let clients = session.clients_for(connection_id);
                if clients.is_empty() {
                    return Ok(());
                }
                // Fan out concurrently so one slow client cannot delay the
                // others for the whole delivery deadline. The source still
                // waits for all destinations before reading its next frame, so
                // per-destination ordering is preserved.
                let deliveries = clients
                    .iter()
                    .map(|client| deliver_one(state, client, opcode, payload));
                let results = futures_util::future::join_all(deliveries).await;
                let delivered = results.iter().filter(|result| result.is_ok()).count();
                if delivered == 0 {
                    return Err((1013, "Delivery unavailable"));
                }
            }
        }
    }
    Ok(())
}

async fn deliver_one(
    state: &Arc<AppState>,
    destination: &crate::session::SocketHandle,
    opcode: OpCode,
    payload: &Bytes,
) -> Result<(), &'static str> {
    match destination
        .writer
        .send_frame(opcode, payload.clone(), state.config.delivery_timeout)
        .await
    {
        Ok(()) => Ok(()),
        Err(error) => {
            match &error {
                SendError::SlowConsumer => {
                    Metrics::inc(&state.metrics.slow_consumer_disconnects);
                    Metrics::inc(&state.metrics.delivery_timeouts);
                }
                SendError::Writer(_) => {
                    // The writer task's deadline or transport error; not counted
                    // as a slow consumer because it may be a destination close.
                    Metrics::inc(&state.metrics.delivery_timeouts);
                }
                SendError::Closed | SendError::TooLarge => {}
            }
            tracing::debug!(
                error = %error,
                destination = destination.writer.id,
                "destination shed"
            );
            destination.writer.close(1013, "Slow consumer");
            Err("Delivery unavailable")
        }
    }
}

/// Elixir-compatible control watchdog for a v2 client whose data socket has not
/// attached yet. This runs independently of client frame delivery: after
/// `CONTROL_NUDGE_MS` it re-sends `sync`, and after `CONTROL_UNRESPONSIVE_MS`
/// it closes a still-unresponsive control socket with `1011`. It never
/// shortens the client's own `DATA_ATTACH_TIMEOUT_MS` wait.
fn spawn_control_watchdog(
    state: Arc<AppState>,
    session: Arc<Session>,
    connection_id: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(state.config.control_nudge_after).await;
        if !session.waiting_for_data(&connection_id) {
            return;
        }
        if let Some(control) = session.control() {
            let sync = json!({"type": "sync", "connectionIds": session.connection_ids()});
            if control
                .writer
                .send_json(&sync, state.config.delivery_timeout)
                .await
                .is_err()
            {
                Metrics::inc(&state.metrics.slow_consumer_disconnects);
                control.writer.close(1013, "Slow consumer");
                return;
            }
        }
        let remaining = state
            .config
            .control_unresponsive_after
            .saturating_sub(state.config.control_nudge_after);
        tokio::time::sleep(remaining).await;
        if session.waiting_for_data(&connection_id) {
            if let Some(control) = session.control() {
                control.writer.close(1011, "Control unresponsive");
            }
        }
    })
}

async fn handle_control_input(
    state: &Arc<AppState>,
    opcode: OpCode,
    control: &Option<crate::session::SocketHandle>,
    payload: &Bytes,
) {
    if opcode != OpCode::Text {
        return;
    }
    let Some(control) = control else {
        return;
    };
    let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(payload) else {
        return;
    };
    if object.get("type").and_then(Value::as_str) != Some("ping") {
        return;
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let pong = json!({"type": "pong", "ts": timestamp});
    if control
        .writer
        .send_json(&pong, state.config.delivery_timeout)
        .await
        .is_err()
    {
        Metrics::inc(&state.metrics.slow_consumer_disconnects);
        control.writer.close(1013, "Slow consumer");
    }
}

fn echo_close_code(code: u16) -> u16 {
    if ws::is_allowed_close_code(code) {
        code
    } else {
        1000
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_reader<R>(
    read: R,
    limits: WsLimits,
    tx: mpsc::Sender<Inbound>,
    capacity: Arc<crate::capacity::Capacity>,
    metrics: Arc<Metrics>,
    wait_timeout: Duration,
    frame_read_timeout: Duration,
    message_read_timeout: Duration,
    ingress_scope: Arc<ConnIngress>,
) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        read_loop(
            read,
            limits,
            tx,
            capacity,
            metrics,
            wait_timeout,
            frame_read_timeout,
            message_read_timeout,
            ingress_scope,
        )
        .await;
    })
}

#[allow(clippy::too_many_arguments)]
async fn read_loop<R>(
    read: R,
    limits: WsLimits,
    tx: mpsc::Sender<Inbound>,
    capacity: Arc<crate::capacity::Capacity>,
    metrics: Arc<Metrics>,
    wait_timeout: Duration,
    frame_read_timeout: Duration,
    message_read_timeout: Duration,
    ingress_scope: Arc<ConnIngress>,
) where
    R: AsyncRead + Unpin,
{
    let mut reader = FrameReader::new(read, limits);
    let mut assembling: Option<Assembling> = None;
    let mut message_started: Option<tokio::time::Instant> = None;

    macro_rules! fail {
        ($failure:expr) => {{
            let _ = tx.send(Inbound::Failed($failure)).await;
            return;
        }};
    }

    loop {
        let header_timeout =
            message_started.map(|started| message_read_timeout.saturating_sub(started.elapsed()));
        if header_timeout == Some(Duration::ZERO) {
            fail!(Failure::ReadTimeout);
        }
        let header = match read_header_bounded(&mut reader, header_timeout).await {
            Ok(header) => header,
            Err(failure) => fail!(failure),
        };
        // Start the fragmented-message clock as soon as the first fragment
        // header arrives, so MESSAGE_READ_TIMEOUT_MS bounds the whole wire
        // time rather than only the time after the first payload was read.
        if matches!(header.opcode, OpCode::Text | OpCode::Binary)
            && !header.fin
            && message_started.is_none()
        {
            message_started = Some(tokio::time::Instant::now());
        }
        let payload_timeout = message_started
            .map(|started| {
                frame_read_timeout.min(message_read_timeout.saturating_sub(started.elapsed()))
            })
            .unwrap_or(frame_read_timeout);
        if payload_timeout == Duration::ZERO {
            fail!(Failure::ReadTimeout);
        }
        let payload_deadline = tokio::time::Instant::now() + payload_timeout;

        match header.opcode {
            OpCode::Close => {
                let payload = match read_small_payload(&mut reader, &header, payload_timeout).await
                {
                    Ok(payload) => payload,
                    Err(failure) => fail!(failure),
                };
                match ws::parse_close_payload(&payload) {
                    Ok((code, reason)) => {
                        let _ = tx.send(Inbound::Close { code, reason }).await;
                    }
                    Err(error) => {
                        let _ = tx.send(Inbound::Failed(Failure::Ws(error))).await;
                    }
                }
                return;
            }
            OpCode::Ping => {
                let payload = match read_small_payload(&mut reader, &header, payload_timeout).await
                {
                    Ok(payload) => payload,
                    Err(failure) => fail!(failure),
                };
                if tx.send(Inbound::Ping(payload)).await.is_err() {
                    return;
                }
            }
            OpCode::Pong => {
                if let Err(failure) =
                    read_small_payload(&mut reader, &header, payload_timeout).await
                {
                    fail!(failure);
                }
                if tx.send(Inbound::Pong).await.is_err() {
                    return;
                }
            }
            OpCode::Text | OpCode::Binary => {
                if assembling.is_some() {
                    fail!(Failure::Ws(WsError::Protocol(
                        "new data frame during fragmented message",
                    )));
                }
                // Charge the fixed per-message overhead once per message. It is
                // charged before the payload is read, but it is small (~1 KiB
                // weighted at the default weight).
                let overhead = match bounded_budget_wait(payload_deadline, wait_timeout) {
                    Ok(wait) => match reserve_weighted(
                        &capacity,
                        &ingress_scope,
                        capacity.ingress.overhead_weight(),
                        wait,
                    )
                    .await
                    {
                        Ok(guard) => guard,
                        Err(failure) => fail!(failure),
                    },
                    Err(failure) => fail!(failure),
                };
                let (payload, mut guards) = match read_data_payload(
                    &mut reader,
                    &header,
                    &capacity,
                    &ingress_scope,
                    wait_timeout,
                    payload_deadline,
                )
                .await
                {
                    Ok(value) => value,
                    Err(failure) => fail!(failure),
                };
                guards.insert(0, overhead);
                metrics.observe_frame_size(header.payload_len);
                if header.fin {
                    if header.opcode == OpCode::Text && std::str::from_utf8(&payload).is_err() {
                        fail!(Failure::Ws(WsError::InvalidUtf8));
                    }
                    if tx
                        .send(Inbound::Message {
                            opcode: header.opcode,
                            payload: Bytes::from(payload),
                            guards,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                } else {
                    assembling = Some(Assembling {
                        opcode: header.opcode,
                        payload,
                        guards,
                    });
                }
            }
            OpCode::Continuation => {
                if let Some(started) = message_started {
                    if started.elapsed() > message_read_timeout {
                        fail!(Failure::ReadTimeout);
                    }
                }
                let Some(mut message) = assembling.take() else {
                    fail!(Failure::Ws(WsError::Protocol("unexpected continuation")));
                };
                let (payload, guards) = match read_data_payload(
                    &mut reader,
                    &header,
                    &capacity,
                    &ingress_scope,
                    wait_timeout,
                    payload_deadline,
                )
                .await
                {
                    Ok(value) => value,
                    Err(failure) => fail!(failure),
                };
                if message.payload.len().saturating_add(payload.len()) > limits.max_message_payload
                {
                    fail!(Failure::Ws(WsError::MessageTooLarge));
                }
                metrics.observe_frame_size(header.payload_len);
                message.payload.extend_from_slice(&payload);
                message.guards.extend(guards);
                if header.fin {
                    message_started = None;
                    if let Err(failure) = finish_message(&tx, message, &limits).await {
                        let _ = tx.send(failure).await;
                        return;
                    }
                } else {
                    assembling = Some(message);
                }
            }
        }
    }
}

/// Read one data-frame payload in [`INGRESS_CHUNK_BYTES`] chunks, charging the
/// ingress budget as bytes actually arrive.
///
/// At most one chunk per connection is transiently allocated before it is
/// charged. A peer that declares a large frame but never sends it therefore
/// holds only the per-message overhead, not the whole declared payload.
async fn read_data_payload<R>(
    reader: &mut FrameReader<R>,
    header: &crate::ws::FrameHeader,
    capacity: &Arc<crate::capacity::Capacity>,
    ingress_scope: &Arc<ConnIngress>,
    wait_timeout: Duration,
    deadline: tokio::time::Instant,
) -> Result<(Vec<u8>, Vec<ScopedGuard>), Failure>
where
    R: AsyncRead + Unpin,
{
    let mut remaining = reader.begin_payload(header).map_err(Failure::Ws)?;
    // Grow the assembled payload only after each chunk has been charged, so the
    // only uncharged memory per in-flight read is the 8 KiB chunk buffer below.
    let mut payload = Vec::new();
    let mut guards = Vec::new();
    while remaining > 0 {
        let chunk_len = remaining.min(INGRESS_CHUNK_BYTES);
        let mut chunk = vec![0u8; chunk_len];
        let read =
            match tokio::time::timeout_at(deadline, reader.read_payload_chunk(&mut chunk)).await {
                Ok(Ok(read)) => read,
                Ok(Err(error)) => return Err(Failure::Ws(error)),
                Err(_) => return Err(Failure::ReadTimeout),
            };
        if read != chunk_len {
            return Err(Failure::Ws(WsError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "short WebSocket payload chunk",
            ))));
        }
        let wait = bounded_budget_wait(deadline, wait_timeout)?;
        let guard = reserve_weighted(
            capacity,
            ingress_scope,
            capacity.ingress.chunk_weight(chunk_len),
            wait,
        )
        .await?;
        payload.extend_from_slice(&chunk);
        guards.push(guard);
        remaining -= chunk_len;
    }
    Ok((payload, guards))
}

/// Remaining budget wait, bounded by the frame/message read deadline so a
/// source cannot extend its read deadline by waiting for ingress capacity.
fn bounded_budget_wait(
    deadline: tokio::time::Instant,
    wait_timeout: Duration,
) -> Result<Duration, Failure> {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        Err(Failure::ReadTimeout)
    } else {
        Ok(wait_timeout.min(remaining))
    }
}

async fn reserve_weighted(
    capacity: &Arc<crate::capacity::Capacity>,
    ingress_scope: &Arc<ConnIngress>,
    weighted: u64,
    wait_timeout: Duration,
) -> Result<ScopedGuard, Failure> {
    let guard = capacity
        .ingress
        .reserve_weighted(weighted, wait_timeout)
        .await
        .map_err(Failure::Budget)?;
    let previous = ingress_scope.used.fetch_add(weighted, Ordering::Relaxed);
    if previous.saturating_add(weighted) > ingress_scope.limit {
        ingress_scope.used.fetch_sub(weighted, Ordering::Relaxed);
        return Err(Failure::Budget(BudgetError::Exhausted));
    }
    Ok(ScopedGuard {
        guard: Some(guard),
        scope: Arc::clone(ingress_scope),
        weighted,
    })
}

struct Assembling {
    opcode: OpCode,
    payload: Vec<u8>,
    guards: Vec<ScopedGuard>,
}

async fn read_header_bounded<R>(
    reader: &mut FrameReader<R>,
    timeout: Option<Duration>,
) -> Result<crate::ws::FrameHeader, Failure>
where
    R: AsyncRead + Unpin,
{
    match timeout {
        Some(remaining) => match tokio::time::timeout(remaining, reader.read_header()).await {
            Ok(Ok(header)) => Ok(header),
            Ok(Err(error)) => Err(Failure::Ws(error)),
            Err(_) => Err(Failure::ReadTimeout),
        },
        None => reader.read_header().await.map_err(Failure::Ws),
    }
}

/// Read a small control payload (close/ping/pong, at most 125 bytes).
async fn read_small_payload<R>(
    reader: &mut FrameReader<R>,
    header: &crate::ws::FrameHeader,
    timeout: Duration,
) -> Result<Bytes, Failure>
where
    R: AsyncRead + Unpin,
{
    match tokio::time::timeout(timeout, reader.read_payload(header)).await {
        Ok(Ok(payload)) => Ok(payload),
        Ok(Err(error)) => Err(Failure::Ws(error)),
        Err(_) => Err(Failure::ReadTimeout),
    }
}

async fn finish_message(
    tx: &mpsc::Sender<Inbound>,
    message: Assembling,
    limits: &WsLimits,
) -> Result<(), Inbound> {
    if message.payload.len() > limits.max_message_payload {
        return Err(Inbound::Failed(Failure::Ws(WsError::MessageTooLarge)));
    }
    if message.opcode == OpCode::Text && std::str::from_utf8(&message.payload).is_err() {
        return Err(Inbound::Failed(Failure::Ws(WsError::InvalidUtf8)));
    }
    tx.send(Inbound::Message {
        opcode: message.opcode,
        payload: Bytes::from(message.payload),
        guards: message.guards,
    })
    .await
    .map_err(|_| Inbound::Close {
        code: 1000,
        reason: String::new(),
    })
}
