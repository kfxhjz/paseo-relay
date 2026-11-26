//! Node-local session topology.
//!
//! One `Session` exists per `serverId` and stores only socket handles and
//! routing metadata; payload bytes never enter this structure. A `DashMap`
//! shards lookups across cores, and each session has a small mutex-protected
//! state.
//!
//! A session is removed as soon as its last socket has left and no connection
//! task still references it, so `PASEO_RELAY_MAX_SESSIONS` counts sessions
//! that somebody is attached to. The periodic idle sweep remains as a safety
//! net for a session whose last reference was dropped without a release.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::config::Config;
use crate::protocol::Connection;
use crate::writer::WriterHandle;

#[derive(Debug, Clone)]
pub struct SocketHandle {
    pub id: u64,
    pub connection: Connection,
    pub writer: WriterHandle,
}

impl SocketHandle {
    pub fn close(&self, code: u16, reason: impl Into<String>) {
        self.writer.close(code, reason);
    }
}

#[derive(Debug)]
pub struct CloseAction {
    pub socket: SocketHandle,
    pub code: u16,
    pub reason: &'static str,
}

#[derive(Debug, Default)]
pub struct AttachResult {
    /// Sockets displaced by this attach; the caller closes them with 1008.
    pub replaced: Vec<SocketHandle>,
    /// For a v2 control socket: connection ids the daemon should reconcile.
    pub sync: Option<Vec<String>>,
    /// For a v2 client socket: the control socket to notify with `connected`.
    pub control: Option<SocketHandle>,
    /// True when the caller should start this session's deduplicated control
    /// watchdog task.
    pub start_watchdog: bool,
}

#[derive(Debug, Default)]
pub struct DetachResult {
    pub closes: Vec<CloseAction>,
    pub control_notification: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    #[error("too many clients for this connectionId")]
    ClientLimit,
    #[error("too many connections for this session")]
    SessionConnectionLimit,
    #[error("session closed")]
    Closed,
}

/// Next action for a session's single deduplicated control watchdog task.
#[derive(Debug)]
pub enum WatchdogStep {
    /// No clients are waiting for a data socket; the watchdog has marked
    /// itself stopped and should exit.
    Exit,
    /// Clients are waiting, but no live control socket is attached right now;
    /// sleep until notified of a session state change.
    WaitForChange,
    /// Sleep for this duration (or until notified of a session state change).
    Sleep(Duration),
    /// Re-send one deduplicated `sync` message to `control`.
    SendNudge {
        control: SocketHandle,
        ids: Vec<String>,
    },
    /// Close an unresponsive `control` socket with `1011`.
    CloseUnresponsive { control: SocketHandle },
}

#[derive(Debug)]
struct SessionState {
    v1_server: Option<SocketHandle>,
    v1_client: Option<SocketHandle>,
    control: Option<SocketHandle>,
    data: std::collections::HashMap<String, SocketHandle>,
    clients: std::collections::HashMap<String, Vec<SocketHandle>>,
    /// Earliest time at which a waiting v2 client attached for each
    /// `connectionId` that still has no daemon data socket.
    waiting_since: std::collections::HashMap<String, Instant>,
    /// Whether the single per-session control watchdog task is currently alive.
    watchdog_running: bool,
    /// When the last nudge `sync` was sent to the current control socket.
    /// A waiting client whose `waiting_since` is after `nudged_at` was not yet
    /// covered by that nudge and must receive its own `sync` before closing
    /// the control socket with `1011`.
    nudged_at: Option<Instant>,
    last_active: Instant,
}

pub struct Session {
    pub server_id: String,
    max_clients_per_connection: usize,
    max_connections_per_session: usize,
    state: Mutex<SessionState>,
    data_ready: Notify,
    watchdog_wake: Arc<Notify>,
}

impl Session {
    fn new(
        server_id: String,
        max_clients_per_connection: usize,
        max_connections_per_session: usize,
    ) -> Self {
        Self {
            server_id,
            max_clients_per_connection,
            max_connections_per_session,
            state: Mutex::new(SessionState {
                v1_server: None,
                v1_client: None,
                control: None,
                data: std::collections::HashMap::new(),
                clients: std::collections::HashMap::new(),
                waiting_since: std::collections::HashMap::new(),
                watchdog_running: false,
                nudged_at: None,
                last_active: Instant::now(),
            }),
            data_ready: Notify::new(),
            watchdog_wake: Arc::new(Notify::new()),
        }
    }

    pub fn watchdog_wake(&self) -> Arc<Notify> {
        Arc::clone(&self.watchdog_wake)
    }

    pub fn attach(&self, socket: SocketHandle) -> Result<AttachResult, AttachError> {
        let mut wake_watchdog = false;
        let result = {
            let mut state = self.state.lock().expect("session state poisoned");
            let now = Instant::now();
            state.last_active = now;
            let connection = socket.connection.clone();
            let mut result = AttachResult::default();

            if connection.version == crate::protocol::Version::V1 {
                let slot = match connection.role {
                    crate::protocol::Role::Server => &mut state.v1_server,
                    crate::protocol::Role::Client => &mut state.v1_client,
                };
                if let Some(old) = slot.replace(socket) {
                    result.replaced.push(old);
                }
            } else if connection.is_v2_control() {
                if let Some(old) = state.control.replace(socket) {
                    result.replaced.push(old);
                }
                // Give a freshly attached control socket a full nudge window
                // starting from its initial sync.
                for since in state.waiting_since.values_mut() {
                    *since = now;
                }
                state.nudged_at = None;
                if !state.waiting_since.is_empty() && !state.watchdog_running {
                    state.watchdog_running = true;
                    result.start_watchdog = true;
                }
                wake_watchdog = true;
                let mut ids: Vec<String> = state.clients.keys().cloned().collect();
                ids.sort();
                result.sync = Some(ids);
            } else if let Some(id) = connection.connection_id() {
                if connection.role == crate::protocol::Role::Server {
                    if !state.data.contains_key(id)
                        && state.data.len() >= self.max_connections_per_session
                    {
                        return Err(AttachError::SessionConnectionLimit);
                    }
                    if let Some(old) = state.data.insert(id.to_owned(), socket) {
                        result.replaced.push(old);
                    }
                    if state.waiting_since.remove(id).is_some() {
                        wake_watchdog = true;
                    }
                } else {
                    if !state.clients.contains_key(id)
                        && state.clients.len() >= self.max_connections_per_session
                    {
                        return Err(AttachError::SessionConnectionLimit);
                    }
                    let clients = state.clients.entry(id.to_owned()).or_default();
                    if clients.len() >= self.max_clients_per_connection {
                        return Err(AttachError::ClientLimit);
                    }
                    // A duplicate socket id is impossible in practice; still
                    // avoid storing the same handle twice.
                    clients.retain(|existing| existing.id != socket.id);
                    clients.push(socket);
                    if !state.data.contains_key(id) {
                        state.waiting_since.entry(id.to_owned()).or_insert(now);
                        if !state.watchdog_running {
                            state.watchdog_running = true;
                            result.start_watchdog = true;
                        }
                        wake_watchdog = true;
                    }
                    result.control = state.control.clone();
                }
            } else {
                return Err(AttachError::Closed);
            }

            result
        };
        if wake_watchdog {
            self.watchdog_wake.notify_one();
        }
        Ok(result)
    }

    pub fn detach(&self, connection: &Connection, socket_id: u64) -> DetachResult {
        let mut wake_watchdog = false;
        let result = {
            let mut result = DetachResult::default();
            let mut state = self.state.lock().expect("session state poisoned");
            state.last_active = Instant::now();

            if connection.version == crate::protocol::Version::V1 {
                let slot = match connection.role {
                    crate::protocol::Role::Server => &mut state.v1_server,
                    crate::protocol::Role::Client => &mut state.v1_client,
                };
                if slot.as_ref().is_some_and(|socket| socket.id == socket_id) {
                    *slot = None;
                }
                return result;
            }

            if connection.is_v2_control() {
                if state
                    .control
                    .as_ref()
                    .is_some_and(|socket| socket.id == socket_id)
                {
                    state.control = None;
                    state.nudged_at = None;
                    wake_watchdog = true;
                }
            } else if let Some(connection_id) = connection.connection_id() {
                if connection.role == crate::protocol::Role::Server {
                    let is_current = state
                        .data
                        .get(connection_id)
                        .is_some_and(|socket| socket.id == socket_id);
                    if is_current {
                        state.data.remove(connection_id);
                        if let Some(clients) = state.clients.get(connection_id) {
                            for client in clients {
                                result.closes.push(CloseAction {
                                    socket: client.clone(),
                                    code: 1012,
                                    reason: "Server disconnected",
                                });
                            }
                        }
                    }
                } else {
                    // v2 client detach.
                    let mut empty_entry = false;
                    if let Some(clients) = state.clients.get_mut(connection_id) {
                        clients.retain(|socket| socket.id != socket_id);
                        empty_entry = clients.is_empty();
                    }
                    let mut notify_disconnect = false;
                    let mut close_data = None;
                    if empty_entry {
                        state.clients.remove(connection_id);
                        if state.waiting_since.remove(connection_id).is_some() {
                            wake_watchdog = true;
                        }
                        notify_disconnect = true;
                        close_data = state.data.get(connection_id).cloned();
                    }
                    if notify_disconnect {
                        if let Some(data) = close_data {
                            result.closes.push(CloseAction {
                                socket: data,
                                code: 1001,
                                reason: "Client disconnected",
                            });
                        }
                        result.control_notification = Some(json!({
                            "type": "disconnected",
                            "connectionId": connection_id,
                        }));
                    }
                }
            }
            result
        };
        if wake_watchdog {
            self.watchdog_wake.notify_one();
        }
        result
    }

    /// Compute the next action for this session's single deduplicated control
    /// watchdog.
    pub fn watchdog_step(
        &self,
        nudge_after: Duration,
        unresponsive_after: Duration,
    ) -> WatchdogStep {
        let mut state = self.state.lock().expect("session state poisoned");
        let SessionState {
            clients,
            data,
            waiting_since,
            ..
        } = &mut *state;
        waiting_since.retain(|id, _| clients.contains_key(id) && !data.contains_key(id));
        if state.waiting_since.is_empty() {
            state.watchdog_running = false;
            state.nudged_at = None;
            return WatchdogStep::Exit;
        }
        let Some(control) = state
            .control
            .as_ref()
            .filter(|socket| !socket.writer.is_closed())
            .cloned()
        else {
            state.nudged_at = None;
            return WatchdogStep::WaitForChange;
        };
        let earliest = state
            .waiting_since
            .values()
            .min()
            .copied()
            .expect("waiting_since is non-empty");
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(earliest);
        let nudged_at = state.nudged_at.filter(|&at| at >= earliest);
        match nudged_at {
            None => {
                if elapsed >= nudge_after {
                    state.nudged_at = Some(now);
                    let mut ids: Vec<String> = state.clients.keys().cloned().collect();
                    ids.sort();
                    WatchdogStep::SendNudge { control, ids }
                } else {
                    WatchdogStep::Sleep(nudge_after.saturating_sub(elapsed))
                }
            }
            Some(nudged_at) => {
                // Always grant the daemon at least `unresponsive_after - nudge_after`
                // after a nudge is emitted, even if the nudge ran late.
                let post_nudge_grace = unresponsive_after.saturating_sub(nudge_after);
                let wait_unresponsive = unresponsive_after.saturating_sub(elapsed);
                let wait_grace =
                    post_nudge_grace.saturating_sub(now.saturating_duration_since(nudged_at));
                let remaining = wait_unresponsive.max(wait_grace);
                if remaining.is_zero() {
                    state.nudged_at = None;
                    WatchdogStep::CloseUnresponsive { control }
                } else {
                    WatchdogStep::Sleep(remaining)
                }
            }
        }
    }

    /// Mark the control watchdog stopped on unexpected exit (e.g. shutdown).
    pub fn mark_watchdog_stopped(&self) {
        let mut state = self.state.lock().expect("session state poisoned");
        state.watchdog_running = false;
        state.nudged_at = None;
    }

    /// Wait until a v2 daemon data socket for `connection_id` is attached.
    pub async fn wait_for_data(
        &self,
        connection_id: &str,
        timeout: Duration,
    ) -> Option<SocketHandle> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.data_ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.state.lock().expect("session state poisoned");
                if let Some(socket) = state.data.get(connection_id) {
                    return Some(socket.clone());
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return None;
            }
        }
    }

    pub fn notify_data_ready(&self) {
        self.data_ready.notify_waiters();
    }

    pub fn v1_peer(&self, role: crate::protocol::Role) -> Option<SocketHandle> {
        let state = self.state.lock().expect("session state poisoned");
        match role {
            crate::protocol::Role::Server => state.v1_client.clone(),
            crate::protocol::Role::Client => state.v1_server.clone(),
        }
    }

    pub fn data_socket(&self, connection_id: &str) -> Option<SocketHandle> {
        let state = self.state.lock().expect("session state poisoned");
        state.data.get(connection_id).cloned()
    }

    pub fn clients_for(&self, connection_id: &str) -> Vec<SocketHandle> {
        let state = self.state.lock().expect("session state poisoned");
        state
            .clients
            .get(connection_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn control(&self) -> Option<SocketHandle> {
        let state = self.state.lock().expect("session state poisoned");
        state.control.clone()
    }

    pub fn is_empty(&self) -> bool {
        let state = self.state.lock().expect("session state poisoned");
        state.v1_server.is_none()
            && state.v1_client.is_none()
            && state.control.is_none()
            && state.data.is_empty()
            && state.clients.is_empty()
    }

    pub fn is_idle(&self, now: Instant, idle: Duration) -> bool {
        let state = self.state.lock().expect("session state poisoned");
        let empty = state.v1_server.is_none()
            && state.v1_client.is_none()
            && state.control.is_none()
            && state.data.is_empty()
            && state.clients.is_empty();
        empty && now.saturating_duration_since(state.last_active) >= idle
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    #[error("session capacity exhausted")]
    SessionLimit,
    #[error("client capacity exhausted for this connectionId")]
    ClientLimit,
    #[error("connection capacity exhausted for this session")]
    SessionConnectionLimit,
}

pub struct SessionRegistry {
    map: DashMap<String, Arc<Session>>,
    active: AtomicU64,
    max_sessions: usize,
    max_clients_per_connection: usize,
    max_connections_per_session: usize,
}

impl SessionRegistry {
    pub fn new(config: &Config) -> Self {
        Self {
            map: DashMap::new(),
            active: AtomicU64::new(0),
            max_sessions: config.max_sessions,
            max_clients_per_connection: config.max_clients_per_connection,
            max_connections_per_session: config.max_connections_per_session,
        }
    }

    pub fn join(
        &self,
        connection: &Connection,
        writer: WriterHandle,
        socket_id: u64,
    ) -> Result<(Arc<Session>, AttachResult), JoinError> {
        let session = match self.map.get(&connection.server_id) {
            Some(existing) => Arc::clone(existing.value()),
            None => match self.map.entry(connection.server_id.clone()) {
                dashmap::mapref::entry::Entry::Occupied(entry) => Arc::clone(entry.get()),
                dashmap::mapref::entry::Entry::Vacant(entry) => {
                    // Reserve the session slot atomically under the entry lock
                    // so concurrent new serverIds cannot overshoot the ceiling.
                    if self
                        .active
                        .try_update(Ordering::SeqCst, Ordering::SeqCst, |active| {
                            (active < self.max_sessions as u64).then_some(active + 1)
                        })
                        .is_err()
                    {
                        return Err(JoinError::SessionLimit);
                    }
                    let new_session = Arc::new(Session::new(
                        connection.server_id.clone(),
                        self.max_clients_per_connection,
                        self.max_connections_per_session,
                    ));
                    entry.insert(Arc::clone(&new_session));
                    new_session
                }
            },
        };

        let handle = SocketHandle {
            id: socket_id,
            connection: connection.clone(),
            writer,
        };

        match session.attach(handle) {
            Ok(attach) => {
                if attach.sync.is_none() && connection.role == crate::protocol::Role::Server {
                    // A daemon data socket may unblock waiting clients.
                    session.notify_data_ready();
                }
                Ok((session, attach))
            }
            Err(err) => Err(match err {
                AttachError::ClientLimit => JoinError::ClientLimit,
                AttachError::SessionConnectionLimit => JoinError::SessionConnectionLimit,
                AttachError::Closed => JoinError::SessionLimit,
            }),
        }
    }

    pub fn leave(&self, connection: &Connection, socket_id: u64) -> DetachResult {
        match self.map.get(&connection.server_id) {
            Some(session) => session.detach(connection, socket_id),
            None => DetachResult::default(),
        }
    }

    /// Remove the session for `server_id` if nobody is attached to it and no
    /// connection task still holds it.
    ///
    /// Callers drop their own `Arc<Session>` first. The check runs under the
    /// map shard lock, and a concurrent `join` clones the `Arc` under the same
    /// lock before attaching, so a session that is about to gain a socket has
    /// a second reference and is kept.
    pub fn release(&self, server_id: &str) {
        let removed = self.map.remove_if(server_id, |_key, session| {
            Arc::strong_count(session) == 1 && session.is_empty()
        });
        if removed.is_some() {
            self.active.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub fn sweep_idle(&self, idle: Duration) {
        let now = Instant::now();
        self.map.retain(|_key, session| {
            let removable = session.is_idle(now, idle) && Arc::strong_count(session) == 1;
            if removable {
                self.active.fetch_sub(1, Ordering::Relaxed);
            }
            !removable
        });
    }

    pub fn active_count(&self) -> u64 {
        self.active.load(Ordering::Relaxed)
    }

    pub fn has_capacity(&self) -> bool {
        self.active.load(Ordering::Relaxed) < self.max_sessions as u64
    }

    pub fn session_capacity_available(&self, server_id: &str) -> bool {
        self.map.contains_key(server_id)
            || self.active.load(Ordering::Relaxed) < self.max_sessions as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;
    use crate::protocol::{Connection, Role, Version};
    use crate::writer::spawn_writer;
    use std::sync::Arc;

    fn conn(role: Role, version: Version, connection_id: Option<&str>) -> Connection {
        Connection {
            server_id: "srv_test".to_owned(),
            role,
            version,
            connection_id: connection_id.map(ToOwned::to_owned),
        }
    }

    fn writer() -> WriterHandle {
        let (_client, server) = tokio::io::duplex(1024);
        let (writer, _task) = spawn_writer(
            server,
            1024 * 1024,
            Duration::from_secs(1),
            Arc::new(Metrics::new()),
        );
        writer
    }

    fn handle(connection: Connection) -> SocketHandle {
        SocketHandle {
            id: 1,
            connection,
            writer: writer(),
        }
    }

    fn registry(max_sessions: usize) -> SessionRegistry {
        SessionRegistry::new(&Config {
            max_sessions,
            ..Config::default()
        })
    }

    #[tokio::test]
    async fn release_removes_an_empty_unreferenced_session() {
        let registry = registry(1);
        let connection = conn(Role::Client, Version::V1, None);
        let (session, _) = registry.join(&connection, writer(), 7).unwrap();
        assert_eq!(registry.active_count(), 1);

        // Still attached: release is a no-op.
        registry.release(&connection.server_id);
        assert_eq!(registry.active_count(), 1);

        registry.leave(&connection, 7);
        // Detached but the connection task still holds its Arc: kept, so a
        // concurrent joiner that cloned the Arc can never be orphaned.
        registry.release(&connection.server_id);
        assert_eq!(registry.active_count(), 1);

        drop(session);
        registry.release(&connection.server_id);
        assert_eq!(registry.active_count(), 0);
        assert!(registry.has_capacity());

        // The slot is reusable at once, without waiting for the idle sweep.
        let other = Connection {
            server_id: "srv_other".to_owned(),
            ..connection
        };
        assert!(registry.join(&other, writer(), 8).is_ok());
    }

    #[tokio::test]
    async fn release_keeps_a_session_another_socket_is_attached_to() {
        let registry = registry(4);
        let server = conn(Role::Server, Version::V1, None);
        let client = conn(Role::Client, Version::V1, None);
        let (server_session, _) = registry.join(&server, writer(), 1).unwrap();
        let (client_session, _) = registry.join(&client, writer(), 2).unwrap();

        registry.leave(&client, 2);
        drop(client_session);
        registry.release(&client.server_id);
        assert_eq!(registry.active_count(), 1);
        assert!(server_session.v1_peer(Role::Client).is_some());
    }

    #[tokio::test]
    async fn v1_replacement_closes_old_but_keeps_other_role() {
        let session = Session::new("srv".into(), 8, 512);
        let first = handle(conn(Role::Server, Version::V1, None));
        session.attach(first.clone()).unwrap();
        let client = handle(conn(Role::Client, Version::V1, None));
        session.attach(client).unwrap();
        let second = handle(conn(Role::Server, Version::V1, None));
        let result = session.attach(second).unwrap();
        assert_eq!(result.replaced.len(), 1);
        assert_eq!(result.replaced[0].id, 1);
        assert!(session.v1_peer(Role::Server).is_some());
    }

    #[tokio::test]
    async fn v2_control_sync_lists_connection_ids() {
        let session = Session::new("srv".into(), 8, 512);
        session
            .attach(handle(conn(Role::Client, Version::V2, Some("conn_a"))))
            .unwrap();
        let control = session
            .attach(handle(conn(Role::Server, Version::V2, Some(""))))
            .unwrap();
        assert_eq!(control.sync, Some(vec!["conn_a".to_owned()]));
    }

    #[tokio::test]
    async fn session_caps_distinct_connection_ids_and_deduplicates_watchdog() {
        let session = Session::new("srv".into(), 8, 2);
        let first = session
            .attach(handle(conn(Role::Client, Version::V2, Some("c1"))))
            .unwrap();
        assert!(first.start_watchdog, "first waiting client starts watchdog");

        let second = session
            .attach(handle(conn(Role::Client, Version::V2, Some("c2"))))
            .unwrap();
        assert!(
            !second.start_watchdog,
            "second waiting client must share the single session watchdog"
        );

        let third = session.attach(handle(conn(Role::Client, Version::V2, Some("c3"))));
        assert!(matches!(third, Err(AttachError::SessionConnectionLimit)));

        // An additional client on an existing connectionId still fits.
        let same_id = SocketHandle {
            id: 2,
            connection: conn(Role::Client, Version::V2, Some("c1")),
            writer: writer(),
        };
        assert!(session.attach(same_id).is_ok());

        // Data sockets are also capped at max_connections_per_session.
        assert!(session
            .attach(handle(conn(Role::Server, Version::V2, Some("c1"))))
            .is_ok());
        assert!(session
            .attach(handle(conn(Role::Server, Version::V2, Some("c2"))))
            .is_ok());
        assert!(matches!(
            session.attach(handle(conn(Role::Server, Version::V2, Some("c3")))),
            Err(AttachError::SessionConnectionLimit)
        ));
    }

    #[tokio::test]
    async fn data_attach_wakes_waiters() {
        let session = Arc::new(Session::new("srv".into(), 8, 512));
        let waiter = {
            let session = Arc::clone(&session);
            tokio::spawn(async move {
                session
                    .wait_for_data("conn_a", Duration::from_secs(2))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        session
            .attach(handle(conn(Role::Server, Version::V2, Some("conn_a"))))
            .unwrap();
        session.notify_data_ready();
        let socket = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert!(socket.is_some());
    }

    #[tokio::test]
    async fn last_client_detach_notifies_and_closes_data() {
        let session = Sequence::new();
        let data = session.attach_data();
        let client = session.attach_client();
        let result = session.session.detach(&client.connection, client.id);
        assert_eq!(result.closes.len(), 1);
        assert_eq!(result.closes[0].code, 1001);
        let notification = result.control_notification.unwrap();
        assert_eq!(notification["type"], "disconnected");
        assert_eq!(notification["connectionId"], "conn_a");
        // The data socket is closed with 1001 but is removed only when it
        // detaches itself, matching the Elixir relay's detach ordering.
        assert!(session.session.data_socket("conn_a").is_some());
        let data_detach = session.session.detach(&data.connection, data.id);
        assert!(data_detach.closes.is_empty());
        assert!(session.session.data_socket("conn_a").is_none());
    }

    #[tokio::test]
    async fn watchdog_nudges_newly_arrived_client_after_earlier_client_resolves() {
        let session = Session::new("srv".into(), 8, 512);
        let control = SocketHandle {
            id: 1,
            connection: conn(Role::Server, Version::V2, Some("")),
            writer: writer(),
        };
        session.attach(control).unwrap();

        let c1 = SocketHandle {
            id: 2,
            connection: conn(Role::Client, Version::V2, Some("c1")),
            writer: writer(),
        };
        session.attach(c1).unwrap();

        // Wait past `nudge_after` and fire the first nudge for `c1`.
        tokio::time::sleep(Duration::from_millis(15)).await;
        assert!(matches!(
            session.watchdog_step(Duration::from_millis(10), Duration::from_millis(60)),
            WatchdogStep::SendNudge { .. }
        ));

        // A second client `c2` arrives AFTER the nudge, and `c1` gets its data
        // socket shortly after.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let c2 = SocketHandle {
            id: 3,
            connection: conn(Role::Client, Version::V2, Some("c2")),
            writer: writer(),
        };
        session.attach(c2).unwrap();
        let d1 = SocketHandle {
            id: 4,
            connection: conn(Role::Server, Version::V2, Some("c1")),
            writer: writer(),
        };
        session.attach(d1).unwrap();

        // Even once `c2` exceeds `unresponsive_after`, it was never covered by
        // `c1`'s earlier sync nudge, so the watchdog must nudge first instead
        // of immediately closing the healthy control socket with 1011.
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            matches!(
                session.watchdog_step(Duration::from_millis(10), Duration::from_millis(20)),
                WatchdogStep::SendNudge { ref ids, .. } if ids == &vec!["c1".to_owned(), "c2".to_owned()]
            ),
            "client arriving after an earlier nudge must receive a sync nudge before 1011"
        );
        // Even right after a late nudge, the daemon still gets the full
        // `unresponsive_after - nudge_after` grace window before `1011`.
        assert!(matches!(
            session.watchdog_step(Duration::from_millis(10), Duration::from_millis(20)),
            WatchdogStep::Sleep(delay) if !delay.is_zero()
        ));
        tokio::time::sleep(Duration::from_millis(15)).await;
        assert!(matches!(
            session.watchdog_step(Duration::from_millis(10), Duration::from_millis(20)),
            WatchdogStep::CloseUnresponsive { .. }
        ));
    }

    struct Sequence {
        session: Arc<Session>,
        next_id: AtomicU64,
    }

    impl Sequence {
        fn new() -> Self {
            Self {
                session: Arc::new(Session::new("srv".into(), 8, 512)),
                next_id: AtomicU64::new(1),
            }
        }

        fn attach_data(&self) -> SocketHandle {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let handle = SocketHandle {
                id,
                connection: conn(Role::Server, Version::V2, Some("conn_a")),
                writer: writer(),
            };
            self.session.attach(handle.clone()).unwrap();
            handle
        }

        fn attach_client(&self) -> SocketHandle {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let handle = SocketHandle {
                id,
                connection: conn(Role::Client, Version::V2, Some("conn_a")),
                writer: writer(),
            };
            self.session.attach(handle.clone()).unwrap();
            handle
        }
    }
}
