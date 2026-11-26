//! HTTP/WebSocket listener, operations endpoints, graceful shutdown.
//!
//! By default one listener serves `/ws` and the operations endpoints. With
//! `PASEO_RELAY_OPS_PORT` set, `/ready`, `/metrics` and `/version` move to a
//! second, plaintext listener that is meant to stay on a private interface;
//! the public listener then answers only `/ws` and `/health`. The operations
//! listener has its own small connection allowance, so probes and scrapes
//! keep working when the public listener is at its TCP cap.

use std::collections::HashSet;
use std::convert::Infallible;
use std::future::Future;
use std::io::{self, IoSlice};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use bytes::Bytes;
use http::{header, Method, StatusCode, Version};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::capacity::{Capacity, TcpRejection, TcpSlot};
use crate::config::Config;
use crate::connection::run_connection;
use crate::metrics::{Gauges, Metrics, ShedKind};
use crate::protocol::Connection;
use crate::session::SessionRegistry;
use crate::ws;

/// Concurrent connections admitted on the operations listener. Probes and
/// scrapes are short requests from a handful of peers.
const OPS_MAX_CONNECTIONS: usize = 64;

/// How often the TLS certificate and key files are checked for a change.
const TLS_WATCH_INTERVAL: Duration = Duration::from_secs(30);

/// Which endpoints a listener answers. `/health` is served everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    /// The public listener. `ops` is false when a dedicated operations
    /// listener exists, which hides `/ready`, `/metrics` and `/version` here.
    Public { ops: bool },
    /// The operations listener: no `/ws`.
    Ops,
}

impl Surface {
    fn serves_ws(self) -> bool {
        matches!(self, Surface::Public { .. })
    }

    fn serves_ops(self) -> bool {
        matches!(self, Surface::Ops | Surface::Public { ops: true })
    }
}

pub struct AppState {
    pub config: Arc<Config>,
    pub allowed_origins: HashSet<String>,
    pub capacity: Arc<Capacity>,
    pub sessions: Arc<SessionRegistry>,
    pub metrics: Arc<Metrics>,
    pub draining: AtomicBool,
    pub shutdown: CancellationToken,
    pub conn_shutdown: CancellationToken,
    pub ws_tasks: TaskTracker,
}

impl AppState {
    fn new(config: Config) -> Arc<Self> {
        let allowed_origins = config.allowed_origins.iter().cloned().collect();
        let config = Arc::new(config);
        let metrics = Arc::new(Metrics::new());
        let capacity = Arc::new(Capacity::new(&config, Arc::clone(&metrics)));
        let sessions = Arc::new(SessionRegistry::new(&config));
        Arc::new(Self {
            draining: AtomicBool::new(config.drain),
            config,
            allowed_origins,
            capacity,
            sessions,
            metrics,
            shutdown: CancellationToken::new(),
            conn_shutdown: CancellationToken::new(),
            ws_tasks: TaskTracker::new(),
        })
    }

    fn ready(&self) -> bool {
        !self.draining.load(Ordering::Relaxed)
            && !self.shutdown.is_cancelled()
            && !self.conn_shutdown.is_cancelled()
            && self.capacity.websocket_capacity_open()
            && self.sessions.has_capacity()
    }
}

pub struct ServerHandle {
    pub addr: SocketAddr,
    ops_addr: Option<SocketAddr>,
    state: Arc<AppState>,
    tls: Option<Arc<TlsReloader>>,
    join: tokio::task::JoinHandle<()>,
    background: Vec<tokio::task::JoinHandle<()>>,
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Address of the operations listener, when one is configured.
    pub fn ops_local_addr(&self) -> Option<SocketAddr> {
        self.ops_addr
    }

    /// Load the TLS certificate and key from their configured paths again.
    ///
    /// New connections use the new certificate; established ones are not
    /// touched. On error the previous certificate stays in use. Returns
    /// `Ok(false)` when the relay runs without native TLS.
    pub fn reload_tls(&self) -> Result<bool> {
        match &self.tls {
            Some(tls) => tls.reload().map(|()| true),
            None => Ok(false),
        }
    }

    /// Async counterpart to [`Self::reload_tls`] that reads PEM files on
    /// Tokio's blocking pool so signal handlers do not block async worker
    /// threads.
    pub async fn reload_tls_async(&self) -> Result<bool> {
        let Some(tls) = self.tls.as_ref().map(Arc::clone) else {
            return Ok(false);
        };
        tokio::task::spawn_blocking(move || tls.reload())
            .await
            .context("tls reload task failed")??;
        Ok(true)
    }

    /// Stop accepting, wait the configured grace period, then close active
    /// sessions with `1012 Relay shutting down`.
    pub async fn shutdown(self) {
        self.state.shutdown.cancel();
        let _ = self.join.await;
        // The accept loop cancels this itself; repeat it so the background
        // tasks stop even if that loop ended abnormally.
        self.state.conn_shutdown.cancel();
        for task in self.background {
            let _ = task.await;
        }
    }
}

/// Bind the listener(s) and start serving in the background. The returned
/// handle exposes the bound addresses (useful when a port is `0` in tests)
/// and a graceful `shutdown`.
pub async fn serve(config: Config) -> Result<ServerHandle> {
    config.validate()?;

    // Install the rustls provider once for TLS mode. Ring is compiled in.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let state = AppState::new(config);
    let listener = TcpListener::bind((state.config.host, state.config.port))
        .await
        .with_context(|| format!("failed to bind {}:{}", state.config.host, state.config.port))?;
    let addr = listener.local_addr()?;
    let ops_listener = match state.config.ops_port {
        Some(port) => Some(
            TcpListener::bind((state.config.ops_host, port))
                .await
                .with_context(|| {
                    format!(
                        "failed to bind operations listener {}:{port}",
                        state.config.ops_host
                    )
                })?,
        ),
        None => None,
    };
    let ops_addr = match &ops_listener {
        Some(listener) => Some(listener.local_addr()?),
        None => None,
    };
    let tls = TlsReloader::new(&state.config)?;
    let surface = Surface::Public {
        ops: ops_listener.is_none(),
    };

    tracing::info!(
        addr = %addr,
        ops = ?ops_addr,
        tls = tls.is_some(),
        "relay listening"
    );

    let mut background = Vec::new();
    if let Some(listener) = ops_listener {
        background.push(tokio::spawn(ops_accept_loop(listener, Arc::clone(&state))));
    }
    if let Some(tls) = &tls {
        background.push(tokio::spawn(tls_watch(
            Arc::clone(tls),
            state.shutdown.clone(),
        )));
    }
    let join = tokio::spawn(accept_loop(
        listener,
        Arc::clone(&state),
        tls.clone(),
        surface,
    ));

    Ok(ServerHandle {
        addr,
        ops_addr,
        state,
        tls,
        join,
        background,
    })
}

fn spawn_public_connection(
    stream: TcpStream,
    peer: SocketAddr,
    state: &Arc<AppState>,
    tls: &Option<Arc<TlsReloader>>,
    surface: Surface,
    tasks: &mut JoinSet<()>,
) {
    let tcp_slot = match state.capacity.try_acquire_tcp(peer.ip()) {
        Ok(slot) => slot,
        Err(rejection) => {
            Metrics::inc(&state.metrics.tcp_rejections);
            let (kind, status, body) = match rejection {
                TcpRejection::Capacity => (
                    ShedKind::TcpCapacity,
                    "503 Service Unavailable",
                    r#"{"error":"TCP capacity"}"#,
                ),
                TcpRejection::PerIp => (
                    ShedKind::PerIpLimit,
                    "429 Too Many Requests",
                    r#"{"error":"Per-IP connection limit"}"#,
                ),
            };
            state.metrics.shed(kind);
            // A TLS client cannot read a plaintext answer, and a
            // handshake is too much work to spend on a peer that
            // is being turned away: just close.
            if tls.is_none() {
                reject_tcp(&stream, status, body);
            }
            return;
        }
    };
    configure_tcp(&stream, state.config.tcp_keepalive);
    let state = Arc::clone(state);
    // Taken per connection so a reloaded certificate applies to
    // the next handshake.
    let tls = tls.as_ref().map(|tls| tls.acceptor());
    // The TCP slot moves into the upgraded connection when the
    // handshake succeeds; otherwise it is released when this task
    // (and thus the holder) is dropped.
    let preupgrade_slot = Arc::new(Mutex::new(Some(tcp_slot)));
    tasks.spawn(async move {
        let slot_holder = Arc::clone(&preupgrade_slot);
        match tls {
            Some(acceptor) => {
                let timeout = state.config.http_header_timeout;
                match tokio::time::timeout(timeout, acceptor.accept(stream)).await {
                    Ok(Ok(tls_stream)) => {
                        serve_http(tls_stream, state, peer, slot_holder, surface).await;
                    }
                    Ok(Err(error)) => {
                        tracing::debug!(error = %error, peer = %peer, "tls handshake failed");
                    }
                    Err(_) => {
                        tracing::debug!(peer = %peer, "tls handshake timeout");
                    }
                }
            }
            None => serve_http(stream, state, peer, slot_holder, surface).await,
        }
    });
}

async fn accept_loop(
    listener: TcpListener,
    state: Arc<AppState>,
    tls: Option<Arc<TlsReloader>>,
    surface: Surface,
) {
    let shutdown = state.shutdown.clone();
    let mut tasks: JoinSet<()> = JoinSet::new();
    let mut sweep = tokio::time::interval(Duration::from_secs(5));
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = sweep.tick() => {
                state.sessions.sweep_idle(state.config.session_idle);
            }
            // Reap finished connection tasks ahead of accepting new sockets so
            // a sustained connect flood cannot accumulate completed JoinSet
            // handles in memory.
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = result {
                    if !error.is_cancelled() {
                        tracing::debug!(error = %error, "connection task ended");
                    }
                }
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(value) => value,
                    Err(error) => {
                        // Accept errors are usually transient (fd exhaustion).
                        tracing::warn!(error = %error, "accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                spawn_public_connection(stream, peer, &state, &tls, surface, &mut tasks);
            }
        }
    }

    state.draining.store(true, Ordering::Relaxed);
    if surface.serves_ops() && !state.config.shutdown_grace.is_zero() {
        // Without a dedicated operations listener, keep the shared listener
        // open through the grace window so orchestrators can observe
        // `/ready` (503), `/health` (200), and `/metrics`; `/ws` already
        // rejects new upgrades with 503 while `draining` is set.
        tracing::info!("draining: rejecting new /ws upgrades during grace window");
        let grace = tokio::time::sleep(state.config.shutdown_grace);
        tokio::pin!(grace);
        loop {
            tokio::select! {
                biased;
                () = &mut grace => break,
                result = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(Err(error)) = result {
                        if !error.is_cancelled() {
                            tracing::debug!(error = %error, "connection task ended");
                        }
                    }
                }
                accepted = listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(value) => value,
                        Err(error) => {
                            tracing::warn!(error = %error, "accept failed during drain");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                    };
                    spawn_public_connection(stream, peer, &state, &tls, surface, &mut tasks);
                }
            }
        }
        drop(listener);
        tracing::info!("draining: listener closed");
    } else {
        drop(listener);
        tracing::info!("draining: listener closed");
        tokio::time::sleep(state.config.shutdown_grace).await;
    }
    state.conn_shutdown.cancel();
    state.ws_tasks.close();
    let _ = tokio::time::timeout(Duration::from_secs(2), state.ws_tasks.wait()).await;

    // Give connection tasks a moment to emit close frames, then abort whatever
    // remains (half-open sockets, idle HTTP keep-alives).
    let deadline = tokio::time::Instant::now() + Duration::from_millis(750);
    while tokio::time::Instant::now() < deadline {
        if state.capacity.active_websockets() == 0 && tasks.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        while tasks.try_join_next().is_some() {}
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    tracing::info!("relay stopped");
}

/// Accept loop of the operations listener.
///
/// It keeps serving through the drain grace window, so an orchestrator can
/// still read `/ready` (503) and `/health` (200) after the public listener
/// has closed, and stops when the remaining sessions are closed.
async fn ops_accept_loop(listener: TcpListener, state: Arc<AppState>) {
    let slots = Arc::new(Semaphore::new(OPS_MAX_CONNECTIONS));
    let mut tasks: JoinSet<()> = JoinSet::new();

    loop {
        tokio::select! {
            biased;
            _ = state.conn_shutdown.cancelled() => break,
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = result {
                    if !error.is_cancelled() {
                        tracing::debug!(error = %error, "operations task ended");
                    }
                }
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::warn!(error = %error, "operations accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
                    reject_tcp(
                        &stream,
                        "503 Service Unavailable",
                        r#"{"error":"Operations capacity"}"#,
                    );
                    continue;
                };
                let _ = stream.set_nodelay(true);
                let state = Arc::clone(&state);
                tasks.spawn(async move {
                    let no_slot = Arc::new(Mutex::new(None));
                    serve_http(stream, state, peer, no_slot, Surface::Ops).await;
                    drop(slot);
                });
            }
        }
    }

    drop(listener);
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

/// Bounds stalled HTTP response writes before a socket upgrades to WebSocket.
///
/// `hyper::server::conn::http1` enforces `header_read_timeout` while reading
/// requests, but imposes no write timeout while flushing responses (for
/// example, a peer that pipelines `/metrics` requests and stops reading). Once
/// a WebSocket upgrade completes, `active` is flipped to `false` so
/// `FrameWriter`'s own delivery timeout governs post-upgrade traffic.
struct PreUpgradeWriteTimeout<S> {
    inner: S,
    timeout: Duration,
    active: Arc<AtomicBool>,
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<S> PreUpgradeWriteTimeout<S> {
    fn new(inner: S, timeout: Duration, active: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            timeout,
            active,
            deadline: None,
        }
    }

    fn poll_write_deadline(&mut self, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        if !self.active.load(Ordering::Relaxed) || self.timeout.is_zero() {
            self.deadline = None;
            return Poll::Pending;
        }
        let sleep = self
            .deadline
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(self.timeout)));
        if sleep.as_mut().poll(cx).is_ready() {
            self.deadline = None;
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "http response write timeout",
            )));
        }
        Poll::Pending
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PreUpgradeWriteTimeout<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PreUpgradeWriteTimeout<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                if n > 0 {
                    this.deadline = None;
                }
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(error)) => {
                this.deadline = None;
                Poll::Ready(Err(error))
            }
            Poll::Pending => match this.poll_write_deadline(cx) {
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                _ => Poll::Pending,
            },
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write_vectored(cx, bufs) {
            Poll::Ready(Ok(n)) => {
                if n > 0 {
                    this.deadline = None;
                }
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(error)) => {
                this.deadline = None;
                Poll::Ready(Err(error))
            }
            Poll::Pending => match this.poll_write_deadline(cx) {
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                _ => Poll::Pending,
            },
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Ready(result) => {
                this.deadline = None;
                Poll::Ready(result)
            }
            Poll::Pending => match this.poll_write_deadline(cx) {
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                _ => Poll::Pending,
            },
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_shutdown(cx) {
            Poll::Ready(result) => {
                this.deadline = None;
                Poll::Ready(result)
            }
            Poll::Pending => match this.poll_write_deadline(cx) {
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                _ => Poll::Pending,
            },
        }
    }
}

async fn serve_http<S>(
    stream: S,
    state: Arc<AppState>,
    peer: SocketAddr,
    preupgrade_slot: Arc<Mutex<Option<TcpSlot>>>,
    surface: Surface,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let header_timeout = state.config.http_header_timeout;
    let write_timeout_active = Arc::new(AtomicBool::new(true));
    let io = TokioIo::new(PreUpgradeWriteTimeout::new(
        stream,
        header_timeout,
        Arc::clone(&write_timeout_active),
    ));
    let service_state = Arc::clone(&state);
    let service_slot = Arc::clone(&preupgrade_slot);
    let service_write_timeout = Arc::clone(&write_timeout_active);
    let service = service_fn(move |request| {
        let state = Arc::clone(&service_state);
        let slot = Arc::clone(&service_slot);
        let write_timeout_active = Arc::clone(&service_write_timeout);
        async move { handle_request(request, state, peer, slot, write_timeout_active, surface).await }
    });
    let connection = http1::Builder::new()
        .header_read_timeout(header_timeout)
        // Bound pre-upgrade memory: a peer cannot make hyper buffer the
        // default ~408 KiB per connection before WebSocket admission.
        .max_buf_size(16 * 1024)
        .max_header_size(16 * 1024)
        .max_headers(64)
        .timer(TokioTimer::new())
        .keep_alive(true)
        .serve_connection(io, service)
        .with_upgrades();
    if let Err(error) = connection.await {
        tracing::debug!(error = %error, peer = %peer, "http connection ended");
    }
}

async fn handle_request(
    request: Request<Incoming>,
    state: Arc<AppState>,
    peer: SocketAddr,
    preupgrade_slot: Arc<Mutex<Option<TcpSlot>>>,
    write_timeout_active: Arc<AtomicBool>,
    surface: Surface,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    let is_get = method == Method::GET || method == Method::HEAD;
    let is_ops = is_get && surface.serves_ops();

    match path.as_str() {
        "/health" if is_get => Ok(json_response(StatusCode::OK, r#"{"status":"ok"}"#)),
        "/ready" if is_ops => {
            if state.ready() {
                Ok(json_response(StatusCode::OK, r#"{"status":"ready"}"#))
            } else {
                Ok(json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    r#"{"status":"unready"}"#,
                ))
            }
        }
        "/version" if is_ops => Ok(json_response(
            StatusCode::OK,
            &format!(
                r#"{{"version":"{}","revision":"{}"}}"#,
                crate::VERSION,
                crate::BUILD_REVISION
            ),
        )),
        "/metrics" if is_ops => Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/plain; version=0.0.4")
            .body(Full::new(Bytes::from(state.metrics.render(Gauges {
                active_websockets: state.capacity.active_websockets(),
                active_sessions: state.sessions.active_count(),
                ingress_reserved_bytes: state.capacity.ingress_reserved_bytes(),
                ingress_budget_bytes: state.capacity.ingress.limit(),
                ingress_small_reserved_bytes: state.capacity.ingress.small_reserved_bytes(),
                ingress_small_budget_bytes: state.capacity.ingress.small_limit(),
                ready: state.ready(),
                draining: state.draining.load(Ordering::Relaxed),
            }))))
            .expect("valid metrics response")),
        "/ws" if method == Method::GET && surface.serves_ws() => {
            Ok(handle_ws(request, state, peer, preupgrade_slot, write_timeout_active).await)
        }
        _ => Ok(text_response(StatusCode::NOT_FOUND, "not found\n")),
    }
}

async fn handle_ws(
    mut request: Request<Incoming>,
    state: Arc<AppState>,
    peer: SocketAddr,
    preupgrade_slot: Arc<Mutex<Option<TcpSlot>>>,
    write_timeout_active: Arc<AtomicBool>,
) -> Response<Full<Bytes>> {
    // Fail fast while draining; no session or capacity state is touched.
    if state.draining.load(Ordering::Relaxed)
        || state.shutdown.is_cancelled()
        || state.conn_shutdown.is_cancelled()
        || state.ws_tasks.is_closed()
    {
        Metrics::inc(&state.metrics.connection_rejections);
        return json_response(StatusCode::SERVICE_UNAVAILABLE, r#"{"status":"unready"}"#);
    }

    if request.version() != Version::HTTP_11 {
        Metrics::inc(&state.metrics.connection_rejections);
        return text_response(StatusCode::BAD_REQUEST, "WebSocket requires HTTP/1.1\n");
    }

    if !origin_allowed(request.headers(), &state.allowed_origins) {
        Metrics::inc(&state.metrics.connection_rejections);
        return text_response(StatusCode::FORBIDDEN, "origin not allowed\n");
    }

    // Complete upgrade validation happens BEFORE any ownership/session/capacity
    // state is created. This closes the "failed upgrade creates an owner" hole.
    let key = match ws::validate_upgrade(request.headers()) {
        Ok(key) => key,
        Err(ws::UpgradeError::NotUpgrade) => {
            Metrics::inc(&state.metrics.connection_rejections);
            return text_response(StatusCode::UPGRADE_REQUIRED, "Expected WebSocket upgrade\n");
        }
        Err(_) => {
            Metrics::inc(&state.metrics.connection_rejections);
            return text_response(StatusCode::BAD_REQUEST, "Invalid WebSocket handshake\n");
        }
    };

    let query = request.uri().query().unwrap_or("");
    let connection = match Connection::from_query(query) {
        Ok(connection) => connection,
        Err(error) => {
            return text_response(StatusCode::BAD_REQUEST, &format!("{}\n", error));
        }
    };

    let Some(ws_slot) = state.capacity.try_acquire_ws() else {
        state.metrics.shed(ShedKind::WebsocketCapacity);
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":"Relay connection capacity"}"#,
        );
    };

    if !state
        .sessions
        .session_capacity_available(&connection.server_id)
    {
        Metrics::inc(&state.metrics.session_rejections);
        state.metrics.shed(ShedKind::SessionCapacity);
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":"Relay session capacity"}"#,
        );
    }

    let ip_guard = match state.capacity.try_acquire_ip(peer.ip()) {
        Ok(guard) => guard,
        Err(_) => {
            state.metrics.shed(ShedKind::PerIpLimit);
            return json_response(
                StatusCode::TOO_MANY_REQUESTS,
                r#"{"error":"Per-IP connection limit"}"#,
            );
        }
    };

    let accept = ws::accept_key(&key);
    let on_upgrade = hyper::upgrade::on(&mut request);
    let response = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header("sec-websocket-accept", accept)
        .body(Full::new(Bytes::new()))
        .expect("valid websocket upgrade response");

    // Keep the pre-upgrade TCP slot in `preupgrade_slot` until `on_upgrade`
    // actually succeeds, so a stalled 101 write never releases the TCP / per-IP
    // slot while `serve_http` still holds the OS socket open.
    let upgrade_timeout = state.config.http_header_timeout.max(Duration::from_secs(1));
    let state = Arc::clone(&state);
    let tracker = state.ws_tasks.clone();
    tracker.spawn(async move {
        match tokio::time::timeout(upgrade_timeout, on_upgrade).await {
            Ok(Ok(upgraded)) => {
                let tcp_slot = preupgrade_slot
                    .lock()
                    .expect("pre-upgrade slot mutex poisoned")
                    .take();
                // Hand write-timeout ownership over to FrameWriter now that
                // the HTTP 101 response has been flushed.
                write_timeout_active.store(false, Ordering::Relaxed);
                run_connection(
                    upgraded, connection, state, ws_slot, ip_guard, tcp_slot, peer,
                )
                .await;
            }
            Ok(Err(error)) => {
                tracing::debug!(error = %error, peer = %peer, "websocket upgrade failed");
                Metrics::inc(&state.metrics.connection_rejections);
            }
            Err(_) => {
                Metrics::inc(&state.metrics.connection_rejections);
            }
        }
    });

    response
}

fn origin_allowed(headers: &http::HeaderMap, allowed: &HashSet<String>) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(value) = origins.next() else {
        // Native Paseo clients (mobile/CLI/daemon) do not send Origin. The
        // allowlist exists to stop browser cross-site WebSocket hijacking, and
        // browsers always send Origin, so an absent header is safe to allow.
        return true;
    };
    // Multiple Origin headers are ambiguous (and browsers never emit more than
    // one); fail closed so a smuggled second Origin cannot slip past checks.
    if origins.next().is_some() {
        return false;
    }
    // A present-but-unparseable Origin cannot match the allowlist; fail closed.
    let Ok(origin) = value.to_str() else {
        return false;
    };
    allowed.contains(origin)
}

fn json_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body.to_owned())))
        .expect("valid response")
}

fn text_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Full::new(Bytes::from(body.to_owned())))
        .expect("valid response")
}

fn configure_tcp(stream: &TcpStream, keepalive: Duration) {
    let _ = stream.set_nodelay(true);
    if keepalive.is_zero() {
        return;
    }
    let sock = socket2::SockRef::from(stream);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(keepalive)
        .with_interval(Duration::from_secs(30));
    let _ = sock.set_tcp_keepalive(&keepalive);
}

/// Answer a socket the relay will not serve, then let it close.
///
/// A freshly accepted socket has an empty send buffer, so this small response
/// is written in full without waiting. It must not wait: this runs inside the
/// accept loop, and a flood of rejected peers is exactly when that loop has
/// to keep moving.
fn reject_tcp(stream: &TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.try_write(response.as_bytes());
}

/// Modification time and size of the certificate and key files.
type FileStamp = [(Option<SystemTime>, u64); 2];

fn file_stamp(cert: &Path, key: &Path) -> Option<FileStamp> {
    let stamp = |path: &Path| {
        let metadata = std::fs::metadata(path).ok()?;
        Some((metadata.modified().ok(), metadata.len()))
    };
    Some([stamp(cert)?, stamp(key)?])
}

struct TlsState {
    acceptor: tokio_rustls::TlsAcceptor,
    /// Stamp of the files behind the acceptor in use.
    loaded: Option<FileStamp>,
    /// Stamp of a pair that failed to load, so it is reported once rather
    /// than on every check.
    rejected: Option<FileStamp>,
}

/// The TLS acceptor in use, replaceable while the relay runs.
///
/// Certificates are renewed far more often than a relay should restart, and a
/// restart drops every session. The files are re-read on `SIGHUP` and whenever
/// their modification time or size changes.
struct TlsReloader {
    cert: PathBuf,
    key: PathBuf,
    /// Serialises reloads (`SIGHUP` vs the periodic watcher) so a slower load
    /// cannot install an older certificate over a newer one.
    reload_lock: Mutex<()>,
    state: Mutex<TlsState>,
}

impl TlsReloader {
    fn new(config: &Config) -> Result<Option<Arc<Self>>> {
        let (Some(cert), Some(key)) = (&config.tls_cert, &config.tls_key) else {
            return Ok(None);
        };
        let stamp = file_stamp(cert, key);
        let acceptor = load_tls(cert, key)?;
        Ok(Some(Arc::new(Self {
            cert: cert.clone(),
            key: key.clone(),
            reload_lock: Mutex::new(()),
            state: Mutex::new(TlsState {
                acceptor,
                loaded: stamp,
                rejected: None,
            }),
        })))
    }

    fn acceptor(&self) -> tokio_rustls::TlsAcceptor {
        self.state
            .lock()
            .expect("tls state poisoned")
            .acceptor
            .clone()
    }

    fn reload_locked(&self) -> Result<()> {
        // Stamp first: if the files change while they are being read, the
        // next check sees a newer stamp and loads again.
        let stamp = file_stamp(&self.cert, &self.key);
        let acceptor = load_tls(&self.cert, &self.key)?;
        let mut state = self.state.lock().expect("tls state poisoned");
        state.acceptor = acceptor;
        state.loaded = stamp;
        state.rejected = None;
        Ok(())
    }

    fn reload(&self) -> Result<()> {
        let _guard = self.reload_lock.lock().expect("tls reload lock poisoned");
        self.reload_locked()
    }

    fn reload_if_changed(&self) {
        let _guard = self.reload_lock.lock().expect("tls reload lock poisoned");
        // A file that is briefly missing mid-rotation is not a change.
        let Some(stamp) = file_stamp(&self.cert, &self.key) else {
            return;
        };
        {
            let state = self.state.lock().expect("tls state poisoned");
            if state.loaded == Some(stamp) || state.rejected == Some(stamp) {
                return;
            }
        }
        match self.reload_locked() {
            Ok(()) => tracing::info!("tls certificate reloaded"),
            Err(error) => {
                self.state.lock().expect("tls state poisoned").rejected = Some(stamp);
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "tls certificate changed on disk but could not be loaded; keeping the previous one"
                );
            }
        }
    }
}

async fn tls_watch(tls: Arc<TlsReloader>, shutdown: CancellationToken) {
    let mut tick = tokio::time::interval(TLS_WATCH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = tick.tick() => {
                let tls = Arc::clone(&tls);
                let _ = tokio::task::spawn_blocking(move || tls.reload_if_changed()).await;
            }
        }
    }
}

fn load_tls(cert_path: &Path, key_path: &Path) -> Result<tokio_rustls::TlsAcceptor> {
    let certs = CertificateDer::pem_file_iter(cert_path)
        .with_context(|| format!("opening TLS certificate {}", cert_path.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parsing TLS certificate chain")?;
    let key = PrivateKeyDer::from_pem_file(key_path)
        .with_context(|| format!("opening TLS key {}", key_path.display()))?;
    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building rustls server config")?;
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(server_config)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn origin_allowlist_matches_exactly_and_fails_closed_on_bad_header() {
        let allowed: HashSet<String> = ["https://app.example".to_owned()].into_iter().collect();
        let empty = HashSet::new();
        assert!(origin_allowed(&http::HeaderMap::new(), &allowed));

        let mut headers = http::HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://app.example"),
        );
        assert!(origin_allowed(&headers, &allowed));

        // Duplicate Origin headers must fail closed even when the first matches.
        headers.append(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert!(!origin_allowed(&headers, &allowed));

        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert!(!origin_allowed(&headers, &allowed));

        headers.insert(header::ORIGIN, HeaderValue::from_bytes(b"\xff").unwrap());
        assert!(!origin_allowed(&headers, &allowed));

        assert!(origin_allowed(&headers, &empty));
    }

    /// A certificate/key pair on disk, rewritten with a new mtime each time.
    struct Pair {
        dir: PathBuf,
        cert: PathBuf,
        key: PathBuf,
        age: u64,
    }

    impl Pair {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "paseo-relay-unit-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let mut pair = Self {
                cert: dir.join("cert.pem"),
                key: dir.join("key.pem"),
                dir,
                age: 0,
            };
            pair.issue();
            pair
        }

        /// Filesystems differ in timestamp resolution, so set distinct
        /// modification times instead of relying on the clock.
        fn touch(&mut self) {
            self.age += 10;
            let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + self.age);
            for path in [&self.cert, &self.key] {
                std::fs::File::options()
                    .write(true)
                    .open(path)
                    .unwrap()
                    .set_modified(time)
                    .unwrap();
            }
        }

        fn issue(&mut self) {
            let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
            std::fs::write(&self.cert, issued.cert.pem()).unwrap();
            std::fs::write(&self.key, issued.signing_key.serialize_pem()).unwrap();
            self.touch();
        }

        fn reloader(&self) -> Arc<TlsReloader> {
            let _ = rustls::crypto::ring::default_provider().install_default();
            TlsReloader::new(&Config {
                tls_cert: Some(self.cert.clone()),
                tls_key: Some(self.key.clone()),
                ..Config::default()
            })
            .unwrap()
            .unwrap()
        }
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn loaded(tls: &TlsReloader) -> Option<FileStamp> {
        tls.state.lock().unwrap().loaded
    }

    fn rejected(tls: &TlsReloader) -> Option<FileStamp> {
        tls.state.lock().unwrap().rejected
    }

    #[test]
    fn tls_watcher_reloads_only_when_the_files_change() {
        let mut pair = Pair::new("watch");
        let tls = pair.reloader();
        let first = loaded(&tls);
        assert!(first.is_some());

        tls.reload_if_changed();
        assert_eq!(loaded(&tls), first, "unchanged files must not be reloaded");

        pair.issue();
        tls.reload_if_changed();
        let second = loaded(&tls);
        assert_ne!(second, first, "a renewed certificate must be picked up");
        assert!(rejected(&tls).is_none());
    }

    #[test]
    fn tls_watcher_keeps_the_old_certificate_when_the_new_one_is_broken() {
        let mut pair = Pair::new("broken");
        let tls = pair.reloader();
        let good = loaded(&tls);

        std::fs::write(&pair.cert, "not a certificate").unwrap();
        pair.touch();
        tls.reload_if_changed();
        assert_eq!(
            loaded(&tls),
            good,
            "a broken pair must not replace a working one"
        );
        let first_rejected = rejected(&tls);
        assert!(first_rejected.is_some());

        // The same broken pair is not retried (and not logged) every tick.
        tls.reload_if_changed();
        assert_eq!(rejected(&tls), first_rejected);

        // A certificate that does not belong to the key is refused too.
        let other = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        std::fs::write(&pair.cert, other.cert.pem()).unwrap();
        pair.touch();
        tls.reload_if_changed();
        assert_eq!(loaded(&tls), good, "a mismatched pair must be refused");

        // Once the pair is whole again it is loaded.
        pair.issue();
        tls.reload_if_changed();
        assert_ne!(loaded(&tls), good);
        assert!(rejected(&tls).is_none());
    }

    #[test]
    fn tls_watcher_ignores_a_file_missing_mid_rotation() {
        let pair = Pair::new("missing");
        let tls = pair.reloader();
        let good = loaded(&tls);
        std::fs::remove_file(&pair.key).unwrap();
        tls.reload_if_changed();
        assert_eq!(loaded(&tls), good);
        assert!(rejected(&tls).is_none());
    }

    #[test]
    fn surfaces_split_public_and_operations_endpoints() {
        let shared = Surface::Public { ops: true };
        assert!(shared.serves_ws() && shared.serves_ops());

        let public = Surface::Public { ops: false };
        assert!(public.serves_ws() && !public.serves_ops());

        assert!(!Surface::Ops.serves_ws() && Surface::Ops.serves_ops());
    }
}
