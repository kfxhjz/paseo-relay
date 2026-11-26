//! Per-WebSocket connection runner.
//!
//! A connection has one read task (frame parsing + ingress reservation) and one
//! writer task (bounded queue). The main task owns session attachment, routing,
//! handshake validation, control notifications, and cleanup. Closing one
//! connection never closes another socket except for the protocol-mandated
//! replacement/detach rules.

use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::Bytes;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::io::AsyncRead;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::capacity::{
    BudgetHold, Capacity, Class, ConnIngress, HoldHandle, IpGuard, Reservation, ScopedGuard,
    TcpSlot, WsSlot,
};
use crate::config::{CONTROL_FRAME_BYTES, INGRESS_CHUNK_BYTES};
use crate::handshake::{self, HandshakeOutcome};
use crate::metrics::{Metrics, ShedKind};
use crate::protocol::{Connection, Role, Version};
use crate::server::AppState;
use crate::session::{JoinError, Session, SocketHandle, WatchdogStep};
use crate::writer::{spawn_writer, ActivityClock, SendError, WriterHandle};
use crate::ws::{
    self, FrameHeader, FrameReader, OpCode, WsError, WsLimits, MAX_FRAGMENTS_PER_MESSAGE,
};

/// Bounded read-ahead: at most two completed messages/events are handed to the
/// main task; the reader task blocks on the channel once that is full.
const READ_CHANNEL_CAPACITY: usize = 2;

/// Attach/finish notifications must never hold a connection teardown for the
/// full delivery deadline (30 s). Bound them so a saturated control writer
/// cannot delay shutdown close frames.
const CONTROL_SEND_TIMEOUT: Duration = Duration::from_secs(1);

enum Inbound {
    Message {
        opcode: OpCode,
        payload: Bytes,
        budget: Arc<BudgetHold>,
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
    /// An ingress reservation could not be made in time, or this connection
    /// reached its own in-flight cap.
    Budget,
    /// A frame or message stalled or ran past its read deadline.
    ReadTimeout,
    /// No frame in either direction for the configured idle timeout.
    Idle,
    /// The in-progress message was reclaimed to free budget for another
    /// connection.
    Reclaimed,
}

impl Failure {
    fn close(&self) -> (u16, String) {
        match self {
            Failure::Ws(error) => (error.close_code(), error.to_string()),
            Failure::Budget => (1013, "Relay ingress capacity".to_owned()),
            Failure::ReadTimeout => (1013, "Relay read timeout".to_owned()),
            Failure::Idle => (1013, "Idle timeout".to_owned()),
            Failure::Reclaimed => (1013, "Relay ingress reclaimed".to_owned()),
        }
    }

    /// Count the failure and let the operator know the node is shedding.
    fn note(&self, metrics: &Metrics) {
        match self {
            Failure::Budget => {
                Metrics::inc(&metrics.ingress_rejections);
                metrics.shed(ShedKind::IngressExhausted);
            }
            Failure::ReadTimeout => {
                Metrics::inc(&metrics.read_timeouts);
                metrics.shed(ShedKind::ReadTimeout);
            }
            Failure::Idle => {
                Metrics::inc(&metrics.idle_timeouts);
                metrics.shed(ShedKind::IdleTimeout);
            }
            // Reclaims are counted where the victim is chosen.
            Failure::Reclaimed | Failure::Ws(_) => {}
        }
    }
}

/// Everything the read task needs besides the socket and its channel.
struct ReadContext {
    limits: WsLimits,
    capacity: Arc<Capacity>,
    metrics: Arc<Metrics>,
    wait_timeout: Duration,
    frame_read_timeout: Duration,
    message_read_timeout: Duration,
    read_stall_timeout: Duration,
    idle_timeout: Option<Duration>,
    ingress_scope: Arc<ConnIngress>,
    hold: HoldHandle,
    /// Outbound activity of this socket; relayed traffic keeps it from being
    /// considered idle.
    activity: Arc<ActivityClock>,
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

    let frame_cap = if connection.is_v2_control() {
        CONTROL_FRAME_BYTES
    } else {
        state.config.max_message_bytes
    };
    let limits = WsLimits {
        max_frame_payload: frame_cap,
        max_message_payload: frame_cap,
        max_fragments: MAX_FRAGMENTS_PER_MESSAGE,
    };

    let one_max_message_weighted = state.config.max_message_weighted_charge();
    // The read-ahead pipeline can hold up to four messages per connection
    // (one being handled by the main task, two queued, one being read), so the
    // per-connection cap must not be lower than that or a legitimate pipelining
    // source would be closed.
    let per_connection_limit = state
        .config
        .ingress_budget_bytes
        .min(one_max_message_weighted.saturating_mul(4))
        .max(one_max_message_weighted);

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
        tx,
        ReadContext {
            limits,
            capacity: Arc::clone(&state.capacity),
            metrics: Arc::clone(&state.metrics),
            wait_timeout: state.config.ingress_wait_timeout,
            frame_read_timeout: state.config.frame_read_timeout,
            message_read_timeout: state.config.message_read_timeout,
            read_stall_timeout: state.config.read_stall_timeout,
            idle_timeout: (!state.config.ws_idle_timeout.is_zero())
                .then_some(state.config.ws_idle_timeout),
            ingress_scope: Arc::new(ConnIngress::new(per_connection_limit)),
            hold: state.capacity.ingress.register(),
            activity: writer.activity(),
        },
    );

    let (session, attach) = match state.sessions.join(&connection, writer.clone(), socket_id) {
        Ok(value) => value,
        Err(error) => {
            Metrics::inc(&state.metrics.session_rejections);
            state.metrics.shed(ShedKind::SessionCapacity);
            let reason = match error {
                JoinError::SessionLimit => "Relay session capacity",
                JoinError::ClientLimit => "Relay client capacity",
                JoinError::SessionConnectionLimit => "Relay session connection capacity",
            };
            writer.close(1013, reason);
            reader_task.abort();
            let _ = reader_task.await;
            let mut writer_task = writer_task;
            if tokio::time::timeout(Duration::from_secs(2), &mut writer_task)
                .await
                .is_err()
            {
                writer_task.abort();
            }
            state.sessions.release(&connection.server_id);
            drop(ws_slot);
            drop(ip_guard);
            return;
        }
    };

    for replaced in attach.replaced {
        replaced.close(1008, "Replaced by new connection");
    }
    if let Some(ids) = attach.sync {
        if let Ok(payload) = serde_json::to_vec(&json!({"type": "sync", "connectionIds": ids})) {
            let _ = writer
                .send_text(Bytes::from(payload), CONTROL_SEND_TIMEOUT)
                .await;
        }
    }
    if let Some(control) = attach.control {
        let connected = json!({
            "type": "connected",
            "connectionId": connection.connection_id().unwrap_or_default(),
        });
        if let Err(error) = control
            .writer
            .send_json(&connected, CONTROL_SEND_TIMEOUT)
            .await
        {
            handle_control_send_err(&state, &control.writer, error);
        }
    }

    if attach.start_watchdog {
        spawn_control_watchdog(
            Arc::clone(&state),
            Arc::downgrade(&session),
            session.watchdog_wake(),
            connection.server_id.clone(),
        );
    }

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
                    Inbound::Message { opcode, payload, budget } => {
                        let outcome = tokio::select! {
                            biased;
                            _ = state.conn_shutdown.cancelled() => {
                                Err((1012, "Relay shutting down"))
                            }
                            _ = writer_cancel.cancelled() => {
                                Err((1013, "Delivery unavailable"))
                            }
                            result = handle_message(
                                &state,
                                &session,
                                &connection,
                                &writer,
                                opcode,
                                &payload,
                                &budget,
                            ) => result,
                        };
                        drop(budget);
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
                        failure.note(&state.metrics);
                        let (code, reason) = failure.close();
                        close_code = code;
                        close_reason = reason;
                        break;
                    }
                }
            }
        }
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

    // Free the session slot now if this was its last socket, instead of
    // leaving an empty session to the idle sweep.
    drop(session);
    state.sessions.release(&connection.server_id);

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
                result = control
                    .writer
                    .send_json(&notification, CONTROL_SEND_TIMEOUT) => {
                    let _ = result;
                }
            }
        }
    }

    writer.close(close_code, close_reason);
    // Abort and wait: the reader owns the budget guards of a partly received
    // message and its registration in the hold registry.
    reader_task.abort();
    let _ = reader_task.await;
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
    self_writer: &WriterHandle,
    opcode: OpCode,
    payload: &Bytes,
    budget: &Arc<BudgetHold>,
) -> Result<(), (u16, &'static str)> {
    if session.is_empty() {
        return Ok(());
    }

    if connection.is_v2_control() {
        handle_control_input(state, opcode, self_writer, payload).await;
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
        Version::V1 => {
            let peer = session.v1_peer(connection.role);
            if let Some(peer) = peer {
                deliver_one(state, &peer, opcode, payload, budget)
                    .await
                    .map_err(|reason| (1013, reason))?;
            }
        }
        Version::V2 => {
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
                deliver_one(state, &destination, opcode, payload, budget)
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
                    .map(|client| deliver_one(state, client, opcode, payload, budget));
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
    destination: &SocketHandle,
    opcode: OpCode,
    payload: &Bytes,
    budget: &Arc<BudgetHold>,
) -> Result<(), &'static str> {
    match destination
        .writer
        .send_frame(
            opcode,
            payload.clone(),
            state.config.delivery_timeout,
            Some(Arc::clone(budget)),
        )
        .await
    {
        Ok(()) => Ok(()),
        Err(error) => {
            match &error {
                SendError::SlowConsumer => {
                    Metrics::inc(&state.metrics.slow_consumer_disconnects);
                    Metrics::inc(&state.metrics.delivery_timeouts);
                    state.metrics.shed(ShedKind::SlowConsumer);
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

/// Close a control socket that could not take a small notification in time.
fn shed_slow_consumer(state: &Arc<AppState>, writer: &WriterHandle) {
    Metrics::inc(&state.metrics.slow_consumer_disconnects);
    state.metrics.shed(ShedKind::SlowConsumer);
    writer.close(1013, "Slow consumer");
}

/// Handle a failed control-socket send without miscounting normal disconnects
/// (`SendError::Closed`) as slow-consumer sheds.
fn handle_control_send_err(state: &Arc<AppState>, writer: &WriterHandle, error: SendError) {
    match error {
        SendError::SlowConsumer => shed_slow_consumer(state, writer),
        SendError::Closed => {}
        SendError::Writer(_) | SendError::TooLarge => writer.close(1013, "Delivery unavailable"),
    }
}

/// Single deduplicated control watchdog per session.
///
/// Holds only a [`Weak<Session>`] across awaits so an empty session can be
/// released immediately when its last socket disconnects. Sends at most one
/// `sync` per `CONTROL_NUDGE_MS` window (bounded by `CONTROL_SEND_TIMEOUT`,
/// with the JSON Value dropped before awaiting), and closes an unresponsive
/// control socket with `1011` after `CONTROL_UNRESPONSIVE_MS`.
fn spawn_control_watchdog(
    state: Arc<AppState>,
    session: Weak<Session>,
    wake: Arc<tokio::sync::Notify>,
    server_id: String,
) {
    tokio::spawn(async move {
        let nudge_after = state.config.control_nudge_after;
        let unresponsive_after = state.config.control_unresponsive_after;

        loop {
            let woken = wake.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();

            let step = {
                let Some(strong) = session.upgrade() else {
                    return;
                };
                let step = strong.watchdog_step(nudge_after, unresponsive_after);
                drop(strong);
                step
            };

            match step {
                WatchdogStep::Exit => {
                    state.sessions.release(&server_id);
                    return;
                }
                WatchdogStep::WaitForChange => {
                    tokio::select! {
                        biased;
                        _ = state.conn_shutdown.cancelled() => {
                            if let Some(strong) = session.upgrade() {
                                strong.mark_watchdog_stopped();
                            }
                            return;
                        }
                        _ = woken => {}
                    }
                }
                WatchdogStep::Sleep(delay) => {
                    tokio::select! {
                        biased;
                        _ = state.conn_shutdown.cancelled() => {
                            if let Some(strong) = session.upgrade() {
                                strong.mark_watchdog_stopped();
                            }
                            return;
                        }
                        _ = woken => {}
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                WatchdogStep::SendNudge { control, ids } => {
                    let payload =
                        serde_json::to_vec(&json!({"type": "sync", "connectionIds": ids}))
                            .map(Bytes::from);
                    if let Ok(payload) = payload {
                        let sent = tokio::select! {
                            biased;
                            _ = state.conn_shutdown.cancelled() => {
                                if let Some(strong) = session.upgrade() {
                                    strong.mark_watchdog_stopped();
                                }
                                return;
                            }
                            result = control.writer.send_text(payload, CONTROL_SEND_TIMEOUT) => result,
                        };
                        if let Err(error) = sent {
                            handle_control_send_err(&state, &control.writer, error);
                        }
                    }
                }
                WatchdogStep::CloseUnresponsive { control } => {
                    control.writer.close(1011, "Control unresponsive");
                }
            }
        }
    });
}

async fn handle_control_input(
    state: &Arc<AppState>,
    opcode: OpCode,
    writer: &WriterHandle,
    payload: &Bytes,
) {
    if opcode != OpCode::Text {
        return;
    }
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
    if let Err(error) = writer.send_json(&pong, state.config.delivery_timeout).await {
        handle_control_send_err(state, writer, error);
    }
}

fn echo_close_code(code: u16) -> u16 {
    if ws::is_allowed_close_code(code) {
        code
    } else {
        1000
    }
}

fn spawn_reader<R>(read: R, tx: mpsc::Sender<Inbound>, ctx: ReadContext) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let evicted = ctx.hold.evicted();
        let reclaimed = {
            let reading = read_loop(read, &tx, &ctx);
            tokio::pin!(reading);
            tokio::select! {
                biased;
                _ = evicted.cancelled() => true,
                _ = &mut reading => false,
            }
            // `reading` is dropped here, and with it the partly received
            // message and the budget guards it held.
        };
        if reclaimed {
            // Unregister immediately rather than keeping the evicted hold in
            // the active map while waiting for `tx.send` under backpressure.
            ctx.hold.message_done();
            let _ = tx.send(Inbound::Failed(Failure::Reclaimed)).await;
        }
    })
}

async fn read_loop<R>(read: R, tx: &mpsc::Sender<Inbound>, ctx: &ReadContext)
where
    R: AsyncRead + Unpin,
{
    let mut reader = FrameReader::new(read, ctx.limits);
    let mut assembling: Option<Assembling> = None;
    let mut message_started: Option<Instant> = None;
    // Last payload progress of the fragmented message being assembled.
    // Control frames and tiny sub-chunk trickles do not advance it.
    let mut last_progress = Instant::now();
    let mut progress_bytes: usize = 0;
    // Last inbound frame of any kind, for the idle timeout.
    let mut last_inbound = Instant::now();

    macro_rules! fail {
        ($failure:expr) => {{
            let _ = tx.send(Inbound::Failed($failure)).await;
            return;
        }};
    }

    loop {
        let header = match message_started {
            Some(started) => {
                // Between fragments the next header must arrive before both
                // the whole-message deadline and the stall deadline.
                let left = ctx
                    .message_read_timeout
                    .saturating_sub(started.elapsed())
                    .min(
                        ctx.read_stall_timeout
                            .saturating_sub(last_progress.elapsed()),
                    );
                if left.is_zero() {
                    fail!(Failure::ReadTimeout);
                }
                match tokio::time::timeout(left, reader.read_header()).await {
                    Ok(Ok(header)) => header,
                    Ok(Err(error)) => fail!(Failure::Ws(error)),
                    Err(_) => fail!(Failure::ReadTimeout),
                }
            }
            None => match read_idle_header(&mut reader, ctx, last_inbound).await {
                Ok(header) => header,
                Err(failure) => fail!(failure),
            },
        };
        last_inbound = Instant::now();

        // Start the fragmented-message clock as soon as the first fragment
        // header arrives, so MESSAGE_READ_TIMEOUT_MS bounds the whole wire
        // time rather than only the time after the first payload was read.
        if matches!(header.opcode, OpCode::Text | OpCode::Binary)
            && !header.fin
            && message_started.is_none()
        {
            message_started = Some(Instant::now());
            last_progress = Instant::now();
            progress_bytes = 0;
        }
        let payload_timeout = message_started
            .map(|started| {
                ctx.frame_read_timeout
                    .min(ctx.message_read_timeout.saturating_sub(started.elapsed()))
            })
            .unwrap_or(ctx.frame_read_timeout);
        if payload_timeout.is_zero() {
            fail!(Failure::ReadTimeout);
        }
        let payload_deadline = Instant::now() + payload_timeout;

        // Control payloads are at most 125 bytes: never allow a truncated
        // Ping/Pong/Close frame to stretch an open message's stall timeout out
        // to the full 300 s frame_read_timeout.
        let control_payload_timeout = || -> Result<Duration, Failure> {
            match message_started {
                Some(_) => {
                    let stall_left = ctx
                        .read_stall_timeout
                        .saturating_sub(last_progress.elapsed());
                    if stall_left.is_zero() {
                        Err(Failure::ReadTimeout)
                    } else {
                        Ok(payload_timeout.min(stall_left))
                    }
                }
                None => Ok(payload_timeout.min(ctx.read_stall_timeout)),
            }
        };

        match header.opcode {
            OpCode::Close => {
                let timeout = match control_payload_timeout() {
                    Ok(timeout) => timeout,
                    Err(failure) => fail!(failure),
                };
                let payload = match read_small_payload(&mut reader, &header, timeout).await {
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
                let timeout = match control_payload_timeout() {
                    Ok(timeout) => timeout,
                    Err(failure) => fail!(failure),
                };
                let payload = match read_small_payload(&mut reader, &header, timeout).await {
                    Ok(payload) => payload,
                    Err(failure) => fail!(failure),
                };
                if tx.send(Inbound::Ping(payload)).await.is_err() {
                    return;
                }
            }
            OpCode::Pong => {
                let timeout = match control_payload_timeout() {
                    Ok(timeout) => timeout,
                    Err(failure) => fail!(failure),
                };
                if let Err(failure) = read_small_payload(&mut reader, &header, timeout).await {
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
                // Only complete single-frame small messages draw from the
                // small reserve; fragmented messages always use the bulk pool
                // so a multi-megabyte transfer cannot lock small-reserve bytes
                // via a <=64 KiB opening fragment.
                let class = if header.fin {
                    Class::for_message_bytes(header.payload_len)
                } else {
                    Class::Bulk
                };
                // Charge the fixed per-message overhead once per message. It is
                // charged before the payload is read, but it is small (~1 KiB
                // weighted at the default weight).
                let overhead = match bounded_budget_wait(payload_deadline, ctx.wait_timeout) {
                    Ok(wait) => {
                        let request = Reservation {
                            weighted: ctx.capacity.ingress.overhead_weight(),
                            class,
                            frame_need: ctx.capacity.ingress.frame_weight(header.payload_len),
                        };
                        match reserve(ctx, request, 0, wait).await {
                            Ok(guard) => guard,
                            Err(failure) => fail!(failure),
                        }
                    }
                    Err(failure) => fail!(failure),
                };
                // Start the stall clock after overhead admission so any
                // relay-imposed budget wait never eats into `read_stall_timeout`.
                last_progress = Instant::now();
                progress_bytes = 0;
                let mut message = Assembling {
                    opcode: header.opcode,
                    payload: Vec::new(),
                    guards: vec![overhead],
                };
                if let Err(failure) = read_data_payload(
                    &mut reader,
                    &header,
                    ctx,
                    class,
                    payload_deadline,
                    &mut message,
                    &mut progress_bytes,
                    &mut last_progress,
                )
                .await
                {
                    fail!(failure);
                }
                ctx.metrics.observe_frame_size(header.payload_len);
                if header.fin {
                    if let Err(failure) = finish_message(tx, message, ctx).await {
                        let _ = tx.send(failure).await;
                        return;
                    }
                } else {
                    assembling = Some(message);
                }
            }
            OpCode::Continuation => {
                if let Some(started) = message_started {
                    if started.elapsed() > ctx.message_read_timeout {
                        fail!(Failure::ReadTimeout);
                    }
                }
                let Some(mut message) = assembling.take() else {
                    fail!(Failure::Ws(WsError::Protocol("unexpected continuation")));
                };
                // The frame reader already enforces the message cap when it
                // parses the header; this keeps the append below safe even if
                // that ever changes.
                let total = message.payload.len().saturating_add(header.payload_len);
                if total > ctx.limits.max_message_payload {
                    fail!(Failure::Ws(WsError::MessageTooLarge));
                }
                let class = Class::Bulk;
                if let Err(failure) = read_data_payload(
                    &mut reader,
                    &header,
                    ctx,
                    class,
                    payload_deadline,
                    &mut message,
                    &mut progress_bytes,
                    &mut last_progress,
                )
                .await
                {
                    fail!(failure);
                }
                ctx.metrics.observe_frame_size(header.payload_len);
                if header.fin {
                    message_started = None;
                    progress_bytes = 0;
                    if let Err(failure) = finish_message(tx, message, ctx).await {
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

/// Read the next frame header while no message is in progress.
///
/// Without an idle timeout this waits indefinitely, which is the documented
/// default. With one, the socket is closed once nothing has been received
/// from it and nothing has been relayed to it for the whole timeout.
async fn read_idle_header<R>(
    reader: &mut FrameReader<R>,
    ctx: &ReadContext,
    last_inbound: Instant,
) -> Result<FrameHeader, Failure>
where
    R: AsyncRead + Unpin,
{
    let Some(idle) = ctx.idle_timeout else {
        return reader.read_header().await.map_err(Failure::Ws);
    };
    // One future for the whole wait: the timer below may fire many times
    // while a header is still arriving, and there is no reason to restart the
    // read each time (`read_header` could be restarted safely, though).
    let header = reader.read_header();
    tokio::pin!(header);
    loop {
        let quiet = last_inbound.elapsed().min(ctx.activity.idle_for());
        let left = idle.saturating_sub(quiet);
        if left.is_zero() {
            return Err(Failure::Idle);
        }
        if let Ok(result) = tokio::time::timeout(left, &mut header).await {
            return result.map_err(Failure::Ws);
        }
    }
}

/// Read one data-frame payload in [`INGRESS_CHUNK_BYTES`] chunks, charging the
/// ingress budget as bytes actually arrive and appending them to `message`.
///
/// At most one chunk per connection is transiently allocated before it is
/// charged. A peer that declares a large frame but never sends it therefore
/// holds only the per-message overhead, not the whole declared payload. Each
/// chunk must also arrive within the stall timeout, so a peer that sends part
/// of a frame and then goes silent does not keep its reservation for the
/// whole frame deadline.
#[allow(clippy::too_many_arguments)]
async fn read_data_payload<R>(
    reader: &mut FrameReader<R>,
    header: &FrameHeader,
    ctx: &ReadContext,
    class: Class,
    deadline: Instant,
    message: &mut Assembling,
    progress_bytes: &mut usize,
    last_progress: &mut Instant,
) -> Result<(), Failure>
where
    R: AsyncRead + Unpin,
{
    let mut remaining = reader.begin_payload(header).map_err(Failure::Ws)?;
    // The message grows only after each chunk has been charged, so the only
    // uncharged memory per in-flight read is this one scratch chunk.
    let mut scratch = vec![0u8; remaining.min(INGRESS_CHUNK_BYTES)];
    while remaining > 0 {
        let chunk_len = remaining.min(INGRESS_CHUNK_BYTES);
        let chunk = &mut scratch[..chunk_len];
        // Bound every read by the remaining stall window since the last full
        // 8 KiB of payload progress, so sending a tiny continuation header near
        // the end of the stall window cannot grant another full stall timeout.
        let read_deadline = deadline.min(*last_progress + ctx.read_stall_timeout);
        let read =
            match tokio::time::timeout_at(read_deadline, reader.read_payload_chunk(chunk)).await {
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
        let wait = bounded_budget_wait(deadline, ctx.wait_timeout)?;
        let request = Reservation {
            weighted: ctx.capacity.ingress.chunk_weight(chunk_len),
            class,
            frame_need: ctx.capacity.ingress.frame_weight(remaining),
        };
        let wait_started = Instant::now();
        let guard = reserve(ctx, request, chunk_len, wait).await?;
        // Relay-imposed budget wait does not count against the sender's stall
        // window.
        let wait_elapsed = wait_started.elapsed();
        if !wait_elapsed.is_zero() {
            *last_progress = (*last_progress + wait_elapsed).min(Instant::now());
        }
        message.payload.extend_from_slice(chunk);
        message.guards.push(guard);
        *progress_bytes = progress_bytes.saturating_add(chunk_len);
        if *progress_bytes >= INGRESS_CHUNK_BYTES {
            *progress_bytes %= INGRESS_CHUNK_BYTES;
            *last_progress = Instant::now();
        }
        remaining -= chunk_len;
    }
    Ok(())
}

/// Remaining budget wait, bounded by the frame/message read deadline so a
/// source cannot extend its read deadline by waiting for ingress capacity.
fn bounded_budget_wait(deadline: Instant, wait_timeout: Duration) -> Result<Duration, Failure> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(Failure::ReadTimeout)
    } else {
        Ok(wait_timeout.min(remaining))
    }
}

/// Reserve node-wide budget for `payload_bytes` newly read bytes and account
/// it to this connection's cap and to its in-progress message.
async fn reserve(
    ctx: &ReadContext,
    request: Reservation,
    payload_bytes: usize,
    wait_timeout: Duration,
) -> Result<ScopedGuard, Failure> {
    let weighted = request.weighted;
    let guard = ctx
        .capacity
        .ingress
        .reserve(&ctx.hold, request, wait_timeout)
        .await
        .map_err(|error| {
            tracing::debug!(error = %error, "ingress reservation failed");
            Failure::Budget
        })?;
    let pool = guard.pool();
    let scoped = ctx
        .ingress_scope
        .try_scope(guard)
        .map_err(|_| Failure::Budget)?;
    ctx.hold.charged(pool, weighted, payload_bytes);
    Ok(scoped)
}

struct Assembling {
    opcode: OpCode,
    payload: Vec<u8>,
    guards: Vec<ScopedGuard>,
}

/// Read a small control payload (close/ping/pong, at most 125 bytes).
async fn read_small_payload<R>(
    reader: &mut FrameReader<R>,
    header: &FrameHeader,
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

/// Validate a complete message and hand it to the main task.
///
/// The message stops being an *incomplete* hold here: from now on its budget
/// is released by delivery, which the relay bounds itself, so it is no longer
/// a candidate for reclaim.
async fn finish_message(
    tx: &mpsc::Sender<Inbound>,
    message: Assembling,
    ctx: &ReadContext,
) -> Result<(), Inbound> {
    if message.payload.len() > ctx.limits.max_message_payload {
        return Err(Inbound::Failed(Failure::Ws(WsError::MessageTooLarge)));
    }
    if message.opcode == OpCode::Text && std::str::from_utf8(&message.payload).is_err() {
        return Err(Inbound::Failed(Failure::Ws(WsError::InvalidUtf8)));
    }
    ctx.hold.message_done();
    tx.send(Inbound::Message {
        opcode: message.opcode,
        payload: Bytes::from(message.payload),
        budget: BudgetHold::new(message.guards),
    })
    .await
    .map_err(|_| Inbound::Close {
        code: 1000,
        reason: String::new(),
    })
}
