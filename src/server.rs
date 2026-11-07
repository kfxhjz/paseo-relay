//! HTTP/WebSocket listener, operations endpoints, graceful shutdown.

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use http::{header, Method, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::capacity::Capacity;
use crate::config::Config;
use crate::connection::run_connection;
use crate::metrics::Metrics;
use crate::protocol::Connection;
use crate::session::SessionRegistry;
use crate::ws;

pub struct AppState {
    pub config: Arc<Config>,
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
        let config = Arc::new(config);
        let metrics = Arc::new(Metrics::new());
        let capacity = Arc::new(Capacity::new(&config, Arc::clone(&metrics)));
        let sessions = Arc::new(SessionRegistry::new(&config));
        Arc::new(Self {
            draining: AtomicBool::new(config.drain),
            config,
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
    state: Arc<AppState>,
    join: tokio::task::JoinHandle<()>,
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop accepting, wait the configured grace period, then close active
    /// sessions with `1012 Relay shutting down`.
    pub async fn shutdown(self) {
        self.state.shutdown.cancel();
        let _ = self.join.await;
    }
}

/// Bind the listener and start serving in the background. The returned handle
/// exposes the bound address (useful when `PASEO_RELAY_PORT=0` in tests) and a
/// graceful `shutdown`.
pub async fn serve(config: Config) -> Result<ServerHandle> {
    config.validate()?;

    // Install the rustls provider once for TLS mode. Ring is compiled in.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let state = AppState::new(config);
    let listener = TcpListener::bind((state.config.host, state.config.port))
        .await
        .with_context(|| format!("failed to bind {}:{}", state.config.host, state.config.port))?;
    let addr = listener.local_addr()?;
    let tls = build_tls(&state.config)?;
    let shutdown = state.shutdown.clone();

    tracing::info!(addr = %addr, tls = tls.is_some(), "relay listening");
    let accept_state = Arc::clone(&state);
    let join = tokio::spawn(async move {
        accept_loop(listener, accept_state, tls, shutdown).await;
    });

    Ok(ServerHandle { addr, state, join })
}

async fn accept_loop(
    listener: TcpListener,
    state: Arc<AppState>,
    tls: Option<tokio_rustls::TlsAcceptor>,
    shutdown: CancellationToken,
) {
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
                let Some(tcp_slot) = state.capacity.try_acquire_tcp() else {
                    reject_tcp(stream).await;
                    continue;
                };
                configure_tcp(&stream, state.config.tcp_keepalive);
                let state = Arc::clone(&state);
                let tls = tls.clone();
                tasks.spawn(async move {
                    match tls {
                        Some(acceptor) => {
                            let timeout = state.config.http_header_timeout;
                            match tokio::time::timeout(timeout, acceptor.accept(stream)).await {
                                Ok(Ok(tls_stream)) => {
                                    serve_http(tls_stream, state, peer).await;
                                }
                                Ok(Err(error)) => {
                                    tracing::debug!(error = %error, peer = %peer, "tls handshake failed");
                                }
                                Err(_) => {
                                    tracing::debug!(peer = %peer, "tls handshake timeout");
                                }
                            }
                        }
                        None => serve_http(stream, state, peer).await,
                    }
                    drop(tcp_slot);
                });
            }
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = result {
                    if !error.is_cancelled() {
                        tracing::debug!(error = %error, "connection task ended");
                    }
                }
            }
        }
    }

    state.draining.store(true, Ordering::Relaxed);
    drop(listener);
    tracing::info!("draining: listener closed");
    tokio::time::sleep(state.config.shutdown_grace).await;
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

async fn serve_http<S>(stream: S, state: Arc<AppState>, peer: SocketAddr)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let io = TokioIo::new(stream);
    let header_timeout = state.config.http_header_timeout;
    let service_state = Arc::clone(&state);
    let service = service_fn(move |request| {
        let state = Arc::clone(&service_state);
        async move { handle_request(request, state, peer).await }
    });
    let connection = http1::Builder::new()
        .header_read_timeout(header_timeout)
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
) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    let is_get = method == Method::GET || method == Method::HEAD;

    match path.as_str() {
        "/health" if is_get => Ok(json_response(StatusCode::OK, r#"{"status":"ok"}"#)),
        "/ready" if is_get => {
            if state.ready() {
                Ok(json_response(StatusCode::OK, r#"{"status":"ready"}"#))
            } else {
                Ok(json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    r#"{"status":"unready"}"#,
                ))
            }
        }
        "/version" if is_get => Ok(json_response(
            StatusCode::OK,
            &format!(
                r#"{{"version":"{}","revision":"{}"}}"#,
                crate::VERSION,
                crate::BUILD_REVISION
            ),
        )),
        "/metrics" if is_get => Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/plain; version=0.0.4")
            .body(Full::new(Bytes::from(state.metrics.render(
                state.capacity.active_websockets(),
                state.sessions.active_count(),
                state.capacity.ingress_reserved_bytes(),
                state.ready(),
                state.draining.load(Ordering::Relaxed),
            ))))
            .expect("valid metrics response")),
        "/ws" if method == Method::GET => Ok(handle_ws(request, state, peer).await),
        _ => Ok(text_response(StatusCode::NOT_FOUND, "not found\n")),
    }
}

async fn handle_ws(
    mut request: Request<Incoming>,
    state: Arc<AppState>,
    peer: SocketAddr,
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

    if !origin_allowed(&request, &state.config.allowed_origins) {
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
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":"Relay session capacity"}"#,
        );
    }

    let ip_guard = match state.capacity.try_acquire_ip(peer.ip()) {
        Ok(guard) => guard,
        Err(_) => {
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

    let state = Arc::clone(&state);
    let tracker = state.ws_tasks.clone();
    tracker.spawn(async move {
        match tokio::time::timeout(Duration::from_secs(10), on_upgrade).await {
            Ok(Ok(upgraded)) => {
                run_connection(upgraded, connection, state, ws_slot, ip_guard, peer).await;
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

fn origin_allowed(request: &Request<Incoming>, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        // Native Paseo clients (mobile/CLI/daemon) do not send Origin. The
        // allowlist exists to stop browser cross-site WebSocket hijacking, and
        // browsers always send Origin, so an absent header is safe to allow.
        return true;
    };
    let set: HashSet<&str> = allowed.iter().map(String::as_str).collect();
    set.contains(origin)
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

async fn reject_tcp(mut stream: TcpStream) {
    let body = br#"{"error":"TCP capacity"}"#;
    let head = format!(
        "HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body).await;
    let _ = stream.shutdown().await;
}

fn build_tls(config: &Config) -> Result<Option<tokio_rustls::TlsAcceptor>> {
    let (Some(cert_path), Some(key_path)) = (&config.tls_cert, &config.tls_key) else {
        return Ok(None);
    };
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
    Ok(Some(tokio_rustls::TlsAcceptor::from(Arc::new(
        server_config,
    ))))
}
