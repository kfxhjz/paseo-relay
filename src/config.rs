//! Runtime configuration loaded from environment variables.
//!
//! The names intentionally mirror the Elixir relay's runtime contract where it
//! makes sense. This implementation is single-node: clustering/reroute settings
//! are deliberately absent (scale with an edge load balancer that hashes on the
//! `serverId` query parameter).

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing_subscriber::EnvFilter;

/// Per-message fixed overhead charged to the ingress budget, in bytes.
///
/// This makes empty or tiny frames non-free: the ledger accounts for the fixed
/// state each in-flight message retains (queue slots, tokens, bookkeeping).
pub const FIXED_MESSAGE_OVERHEAD: u64 = 256;

/// Paseo contract: masked client frames are capped at 32 MiB wire bytes; the
/// header is 14 bytes, so the maximum payload is 32 MiB - 14.
pub const MAX_CLIENT_FRAME_PAYLOAD: usize = 32 * 1024 * 1024 - 14;

/// Default maximum payload of one reassembled data message.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = MAX_CLIENT_FRAME_PAYLOAD;

/// Per-connection inbound frame cap for the v2 daemon control socket.
pub const CONTROL_FRAME_BYTES: usize = 64 * 1024;

/// Data payloads are charged to the ingress budget in chunks of this size.
pub const INGRESS_CHUNK_BYTES: usize = 8 * 1024;

/// Extra weighted charge per payload chunk for its bookkeeping (one
/// connection-scoped budget guard plus vector capacity). This keeps the
/// unaccounted heap amplification of many tiny chunks bounded by the budget.
pub const INGRESS_CHUNK_OVERHEAD: u64 = 192;

/// Maximum number of fragments accepted for one data message.
pub const MAX_FRAGMENTS_PER_MESSAGE: usize = 1024;

/// Largest message (total payload) that may be charged to the small-message
/// reserve. Matches the handshake-inspection and control-frame caps: anything
/// a Paseo handshake or control exchange needs fits.
pub const SMALL_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    pub host: IpAddr,
    pub port: u16,
    /// Listener IP of the optional operations listener.
    pub ops_host: IpAddr,
    /// Port of the operations listener. When set, `/ready`, `/metrics` and
    /// `/version` are served only there and the public listener keeps `/ws`
    /// and `/health`. `None` serves everything on the public listener.
    pub ops_port: Option<u16>,
    pub drain: bool,

    /// Maximum number of upgraded WebSockets.
    pub max_connections: usize,
    /// Maximum number of live sessions (`serverId` entries).
    pub max_sessions: usize,
    /// Maximum number of open TCP connections (pre-upgrade HTTP included).
    pub max_tcp_connections: usize,
    /// Maximum number of client sockets attached to one v2 connectionId.
    pub max_clients_per_connection: usize,
    /// Maximum number of distinct v2 connectionIds (and data routes) in one session.
    pub max_connections_per_session: usize,

    /// Weighted byte budget for admitted, in-flight WebSocket payloads.
    pub ingress_budget_bytes: u64,
    /// Conservative memory weight charged per payload byte.
    pub ingress_weight: u32,
    /// Weighted byte budget reserved for messages of at most
    /// [`SMALL_MESSAGE_BYTES`], in addition to `ingress_budget_bytes`
    /// (0 = no reserve).
    pub ingress_small_reserve_bytes: u64,
    /// Under budget pressure, an in-progress message receiving at or below
    /// this many payload bytes per second is reclaimed first.
    pub ingress_reclaim_min_rate_bytes: u64,
    /// Under budget pressure, an in-progress message that has been arriving
    /// for longer than this yields to a newer one. Zero disables the rule.
    pub ingress_reclaim_after: Duration,
    /// Maximum time to wait for ingress budget before closing the source.
    pub ingress_wait_timeout: Duration,
    /// Maximum payload of one reassembled data message.
    pub max_message_bytes: usize,
    /// Per-socket writer queue bound, in bytes (control queue has its own floor).
    pub control_queue_bytes: usize,

    /// Maximum time a v2 client frame waits for its daemon data socket.
    pub data_attach_timeout: Duration,
    /// Delay before the relay re-sends `sync` to a control socket while a
    /// v2 client is still waiting for its data route.
    pub control_nudge_after: Duration,
    /// Delay after which an unresponsive control socket is closed `1011`.
    pub control_unresponsive_after: Duration,
    /// Maximum time a delivery may wait for a destination writer / write.
    pub delivery_timeout: Duration,
    /// Maximum time to receive one frame payload after its header.
    pub frame_read_timeout: Duration,
    /// Maximum time to assemble one fragmented message.
    pub message_read_timeout: Duration,
    /// Maximum time an in-progress message may go without payload progress
    /// (one 8 KiB chunk, or the next fragment).
    pub read_stall_timeout: Duration,
    /// Close a WebSocket after this long without any frame in either
    /// direction. Zero disables the idle timeout (the default).
    pub ws_idle_timeout: Duration,
    /// Idle time before an empty session is removed.
    pub session_idle: Duration,
    /// Maximum number of WebSocket connections from one peer IP (0 = unlimited).
    pub per_ip_max_connections: usize,
    /// TCP keepalive idle time applied to accepted sockets.
    pub tcp_keepalive: Duration,
    /// Grace period after SIGTERM before existing sessions are closed.
    pub shutdown_grace: Duration,
    /// Timeout for the HTTP header read phase (pre-upgrade slowloris guard).
    pub http_header_timeout: Duration,

    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,

    /// Optional Origin allowlist for `/ws`. Empty means allow any origin.
    pub allowed_origins: Vec<String>,

    /// `tracing` EnvFilter directive (e.g. `info`, `paseo_relay=debug`).
    pub log_filter: String,
    /// Emit JSON logs (useful for Kubernetes/structured log pipelines).
    pub log_json: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".parse().expect("valid loopback IP"),
            port: 4000,
            ops_host: "127.0.0.1".parse().expect("valid loopback IP"),
            ops_port: None,
            drain: false,
            max_connections: 20_000,
            max_sessions: 20_000,
            max_tcp_connections: 25_000,
            max_clients_per_connection: 32,
            max_connections_per_session: 512,
            ingress_budget_bytes: 512 * 1024 * 1024,
            ingress_weight: 4,
            ingress_small_reserve_bytes: 16 * 1024 * 1024,
            ingress_reclaim_min_rate_bytes: 64 * 1024,
            ingress_reclaim_after: Duration::from_millis(60_000),
            ingress_wait_timeout: Duration::from_millis(5_000),
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            control_queue_bytes: 1024 * 1024,
            data_attach_timeout: Duration::from_millis(15_000),
            control_nudge_after: Duration::from_millis(10_000),
            control_unresponsive_after: Duration::from_millis(15_000),
            delivery_timeout: Duration::from_millis(30_000),
            frame_read_timeout: Duration::from_millis(300_000),
            message_read_timeout: Duration::from_millis(600_000),
            read_stall_timeout: Duration::from_millis(30_000),
            ws_idle_timeout: Duration::ZERO,
            session_idle: Duration::from_millis(30_000),
            per_ip_max_connections: 0,
            tcp_keepalive: Duration::from_millis(60_000),
            shutdown_grace: Duration::from_millis(10_000),
            http_header_timeout: Duration::from_millis(15_000),
            tls_cert: None,
            tls_key: None,
            allowed_origins: Vec::new(),
            log_filter: "info".to_owned(),
            log_json: false,
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_vars(std::env::vars())
    }

    /// Parse configuration from an arbitrary key/value iterator. Useful for
    /// tests and for embedding.
    pub fn from_vars<I, K, V>(vars: I) -> Result<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let vars: HashMap<String, String> = vars
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        Self::from_map(&vars)
    }

    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self> {
        let mut cfg = Config::default();

        if let Some(value) = get(vars, "PASEO_RELAY_HOST") {
            cfg.host = value
                .parse()
                .with_context(|| "PASEO_RELAY_HOST must be an IP address")?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_PORT") {
            if value.contains("://") {
                // Kubernetes injects `<SERVICE>_PORT=tcp://<ip>:<port>` into
                // every pod for each Service in the namespace. A Service named
                // `paseo-relay` therefore overwrites this setting.
                bail!(
                    "PASEO_RELAY_PORT is {value:?}, which looks like a Kubernetes service link for a \
                     Service named paseo-relay rather than a port; set PASEO_RELAY_PORT explicitly \
                     in the pod spec or set enableServiceLinks: false"
                );
            }
            cfg.port = parse_u64("PASEO_RELAY_PORT", &value, 1, 65_535)? as u16;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_OPS_HOST") {
            cfg.ops_host = value
                .parse()
                .with_context(|| "PASEO_RELAY_OPS_HOST must be an IP address")?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_OPS_PORT") {
            cfg.ops_port = Some(parse_u64("PASEO_RELAY_OPS_PORT", &value, 1, 65_535)? as u16);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_DRAIN") {
            cfg.drain = parse_bool("PASEO_RELAY_DRAIN", &value)?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_MAX_CONNECTIONS") {
            cfg.max_connections = parse_usize("PASEO_RELAY_MAX_CONNECTIONS", &value, 1, 1_000_000)?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_MAX_SESSIONS") {
            cfg.max_sessions = parse_usize("PASEO_RELAY_MAX_SESSIONS", &value, 1, 1_000_000)?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_MAX_TCP_CONNECTIONS") {
            cfg.max_tcp_connections =
                parse_usize("PASEO_RELAY_MAX_TCP_CONNECTIONS", &value, 1, 1_000_000)?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_MAX_CLIENTS_PER_CONNECTION") {
            cfg.max_clients_per_connection =
                parse_usize("PASEO_RELAY_MAX_CLIENTS_PER_CONNECTION", &value, 1, 65_535)?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION") {
            cfg.max_connections_per_session =
                parse_usize("PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION", &value, 1, 65_535)?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_INGRESS_BUDGET_BYTES") {
            cfg.ingress_budget_bytes = parse_u64(
                "PASEO_RELAY_INGRESS_BUDGET_BYTES",
                &value,
                128 * 1024 * 1024,
                4 * 1024 * 1024 * 1024,
            )?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_INGRESS_WEIGHT") {
            // A weight of 1 cannot cover the transient copy during fragmented
            // message assembly; 4 (the default) is the documented safe floor.
            cfg.ingress_weight = parse_u64("PASEO_RELAY_INGRESS_WEIGHT", &value, 4, 16)? as u32;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_INGRESS_SMALL_RESERVE_BYTES") {
            cfg.ingress_small_reserve_bytes = parse_u64(
                "PASEO_RELAY_INGRESS_SMALL_RESERVE_BYTES",
                &value,
                0,
                1024 * 1024 * 1024,
            )?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_INGRESS_RECLAIM_MIN_RATE_BYTES") {
            cfg.ingress_reclaim_min_rate_bytes = parse_u64(
                "PASEO_RELAY_INGRESS_RECLAIM_MIN_RATE_BYTES",
                &value,
                0,
                64 * 1024 * 1024,
            )?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_INGRESS_RECLAIM_AFTER_MS") {
            cfg.ingress_reclaim_after = Duration::from_millis(parse_u64(
                "PASEO_RELAY_INGRESS_RECLAIM_AFTER_MS",
                &value,
                0,
                600_000,
            )?);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_INGRESS_WAIT_TIMEOUT_MS") {
            cfg.ingress_wait_timeout = Duration::from_millis(parse_u64(
                "PASEO_RELAY_INGRESS_WAIT_TIMEOUT_MS",
                &value,
                10,
                120_000,
            )?);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_MAX_MESSAGE_BYTES") {
            cfg.max_message_bytes = parse_usize(
                "PASEO_RELAY_MAX_MESSAGE_BYTES",
                &value,
                1024,
                MAX_CLIENT_FRAME_PAYLOAD,
            )?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_CONTROL_QUEUE_BYTES") {
            cfg.control_queue_bytes = parse_usize(
                "PASEO_RELAY_CONTROL_QUEUE_BYTES",
                &value,
                64 * 1024,
                64 * 1024 * 1024,
            )?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_DATA_ATTACH_TIMEOUT_MS") {
            cfg.data_attach_timeout = Duration::from_millis(parse_u64(
                "PASEO_RELAY_DATA_ATTACH_TIMEOUT_MS",
                &value,
                100,
                120_000,
            )?);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_DELIVERY_TIMEOUT_MS") {
            cfg.delivery_timeout = Duration::from_millis(parse_u64(
                "PASEO_RELAY_DELIVERY_TIMEOUT_MS",
                &value,
                100,
                300_000,
            )?);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_CONTROL_NUDGE_MS") {
            cfg.control_nudge_after = Duration::from_millis(parse_u64(
                "PASEO_RELAY_CONTROL_NUDGE_MS",
                &value,
                100,
                120_000,
            )?);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_CONTROL_UNRESPONSIVE_MS") {
            cfg.control_unresponsive_after = Duration::from_millis(parse_u64(
                "PASEO_RELAY_CONTROL_UNRESPONSIVE_MS",
                &value,
                100,
                300_000,
            )?);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_FRAME_READ_TIMEOUT_MS") {
            cfg.frame_read_timeout = Duration::from_millis(parse_u64(
                "PASEO_RELAY_FRAME_READ_TIMEOUT_MS",
                &value,
                100,
                300_000,
            )?);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_MESSAGE_READ_TIMEOUT_MS") {
            cfg.message_read_timeout = Duration::from_millis(parse_u64(
                "PASEO_RELAY_MESSAGE_READ_TIMEOUT_MS",
                &value,
                100,
                600_000,
            )?);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_READ_STALL_TIMEOUT_MS") {
            cfg.read_stall_timeout = Duration::from_millis(parse_u64(
                "PASEO_RELAY_READ_STALL_TIMEOUT_MS",
                &value,
                100,
                600_000,
            )?);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_WS_IDLE_TIMEOUT_MS") {
            cfg.ws_idle_timeout = Duration::from_millis(parse_u64(
                "PASEO_RELAY_WS_IDLE_TIMEOUT_MS",
                &value,
                0,
                86_400_000,
            )?);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_SESSION_IDLE_MS") {
            cfg.session_idle = Duration::from_millis(parse_u64(
                "PASEO_RELAY_SESSION_IDLE_MS",
                &value,
                1_000,
                3_600_000,
            )?);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_PER_IP_MAX_CONNECTIONS") {
            cfg.per_ip_max_connections =
                parse_usize("PASEO_RELAY_PER_IP_MAX_CONNECTIONS", &value, 0, 1_000_000)?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_TCP_KEEPALIVE_MS") {
            cfg.tcp_keepalive = Duration::from_millis(parse_u64(
                "PASEO_RELAY_TCP_KEEPALIVE_MS",
                &value,
                0,
                3_600_000,
            )?);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_SHUTDOWN_GRACE_MS") {
            cfg.shutdown_grace = Duration::from_millis(parse_u64(
                "PASEO_RELAY_SHUTDOWN_GRACE_MS",
                &value,
                0,
                300_000,
            )?);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_HTTP_HEADER_TIMEOUT_MS") {
            cfg.http_header_timeout = Duration::from_millis(parse_u64(
                "PASEO_RELAY_HTTP_HEADER_TIMEOUT_MS",
                &value,
                100,
                120_000,
            )?);
        }
        if let Some(value) = get(vars, "PASEO_RELAY_TLS_CERT") {
            cfg.tls_cert = Some(PathBuf::from(value));
        }
        if let Some(value) = get(vars, "PASEO_RELAY_TLS_KEY") {
            cfg.tls_key = Some(PathBuf::from(value));
        }
        if let Some(value) = get(vars, "PASEO_RELAY_ALLOWED_ORIGINS") {
            cfg.allowed_origins = value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect();
        }
        if let Some(value) = get(vars, "PASEO_RELAY_LOG") {
            cfg.log_filter = value;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_LOG_FORMAT") {
            cfg.log_json = match value.as_str() {
                "text" => false,
                "json" => true,
                _ => bail!("PASEO_RELAY_LOG_FORMAT must be text or json"),
            };
        }

        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.max_connections > self.max_tcp_connections {
            bail!("PASEO_RELAY_MAX_CONNECTIONS must be <= PASEO_RELAY_MAX_TCP_CONNECTIONS");
        }
        if self.max_message_bytes > MAX_CLIENT_FRAME_PAYLOAD {
            bail!(
                "PASEO_RELAY_MAX_MESSAGE_BYTES must be <= {}",
                MAX_CLIENT_FRAME_PAYLOAD
            );
        }
        if self.ingress_budget_bytes > u32::MAX as u64 {
            bail!("PASEO_RELAY_INGRESS_BUDGET_BYTES must be <= {}", u32::MAX);
        }
        if self.ingress_weight < 4 || self.ingress_weight > 16 {
            bail!("PASEO_RELAY_INGRESS_WEIGHT must be between 4 and 16");
        }
        let one_max_message = self.max_message_weighted_charge();
        if self.ingress_budget_bytes < one_max_message {
            bail!(
                "PASEO_RELAY_INGRESS_BUDGET_BYTES must admit one maximum message at the configured weight"
            );
        }
        if self.ingress_small_reserve_bytes > u32::MAX as u64 {
            bail!(
                "PASEO_RELAY_INGRESS_SMALL_RESERVE_BYTES must be <= {}",
                u32::MAX
            );
        }
        let one_small_message = self.small_message_weighted_charge();
        if self.ingress_small_reserve_bytes != 0
            && self.ingress_small_reserve_bytes < one_small_message
        {
            bail!(
                "PASEO_RELAY_INGRESS_SMALL_RESERVE_BYTES must be 0 or at least {one_small_message} (one {SMALL_MESSAGE_BYTES}-byte message at the configured weight)"
            );
        }
        if !self.ingress_reclaim_after.is_zero()
            && self.ingress_reclaim_after < Duration::from_secs(5)
        {
            // Shorter than this and ordinary large uploads on a busy node
            // would keep displacing each other.
            bail!("PASEO_RELAY_INGRESS_RECLAIM_AFTER_MS must be 0 (disabled) or at least 5000");
        }
        if !self.ws_idle_timeout.is_zero() && self.ws_idle_timeout < Duration::from_secs(1) {
            bail!("PASEO_RELAY_WS_IDLE_TIMEOUT_MS must be 0 (disabled) or at least 1000");
        }
        if let Some(ops_port) = self.ops_port {
            // Port 0 asks the OS for a free port (tests); it cannot collide.
            let same_family = self.ops_host.is_ipv4() == self.host.is_ipv4();
            let hosts_overlap = self.ops_host == self.host
                || (same_family && (self.host.is_unspecified() || self.ops_host.is_unspecified()));
            if ops_port != 0 && ops_port == self.port && hosts_overlap {
                bail!(
                    "PASEO_RELAY_OPS_PORT must differ from PASEO_RELAY_PORT when the listeners share or overlap on the same address"
                );
            }
        }
        match (&self.tls_cert, &self.tls_key) {
            (Some(cert), Some(key)) => {
                // Catch missing/unreadable files at validate time so
                // `--check-config` catches deployment mistakes.
                check_readable_file(cert, "PASEO_RELAY_TLS_CERT")?;
                check_readable_file(key, "PASEO_RELAY_TLS_KEY")?;
            }
            (None, None) => {}
            _ => bail!("PASEO_RELAY_TLS_CERT and PASEO_RELAY_TLS_KEY must be set together"),
        }

        // The documented runtime contract says invalid configuration aborts
        // startup; validate the log filter instead of silently falling back.
        EnvFilter::try_new(&self.log_filter)
            .context("PASEO_RELAY_LOG must be a valid tracing EnvFilter directive")?;
        if self.max_clients_per_connection == 0 {
            bail!("PASEO_RELAY_MAX_CLIENTS_PER_CONNECTION must be >= 1");
        }
        if self.max_connections_per_session == 0 {
            bail!("PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION must be >= 1");
        }
        if self.control_unresponsive_after <= self.control_nudge_after {
            bail!("PASEO_RELAY_CONTROL_UNRESPONSIVE_MS must be greater than PASEO_RELAY_CONTROL_NUDGE_MS");
        }
        if self.control_queue_bytes == 0 {
            bail!("PASEO_RELAY_CONTROL_QUEUE_BYTES must be > 0");
        }
        Ok(())
    }

    /// Worst-case weighted charge for one maximum-size message.
    pub fn max_message_weighted_charge(&self) -> u64 {
        self.message_weighted_charge(self.max_message_bytes)
    }

    /// Worst-case weighted charge for one message that still qualifies for
    /// the small-message reserve.
    pub fn small_message_weighted_charge(&self) -> u64 {
        self.message_weighted_charge(SMALL_MESSAGE_BYTES.min(self.max_message_bytes))
    }

    /// Worst-case weighted charge for one message of `message_bytes`.
    ///
    /// Includes the per-message overhead, all payload chunks, and the
    /// per-chunk bookkeeping. The extra `MAX_FRAGMENTS_PER_MESSAGE` chunks
    /// cover the worst case where every fragment forms its own partial chunk.
    fn message_weighted_charge(&self, message_bytes: usize) -> u64 {
        let chunks = message_bytes.saturating_add(INGRESS_CHUNK_BYTES - 1) / INGRESS_CHUNK_BYTES;
        let chunks = chunks.saturating_add(MAX_FRAGMENTS_PER_MESSAGE);
        // Each non-empty chunk carries at least one payload byte, so the chunk
        // count can never exceed the message size.
        let chunks = chunks.min(message_bytes) as u64;
        (message_bytes as u64 + FIXED_MESSAGE_OVERHEAD)
            .saturating_mul(self.ingress_weight as u64)
            .saturating_add(chunks.saturating_mul(INGRESS_CHUNK_OVERHEAD))
    }

    /// Bytes available to one destination writer queue.
    ///
    /// A single maximum message must fit so a slow consumer is shed by the
    /// delivery deadline rather than by an unreservable queue slot. The control
    /// queue floor is kept separate so small control notifications are never
    /// starved by a single large payload.
    pub fn writer_queue_bytes(&self) -> usize {
        self.control_queue_bytes
            .max(self.max_message_bytes.saturating_add(64 * 1024))
    }
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{} ops={} max_connections={} max_sessions={} max_connections_per_session={} ingress_budget={} small_reserve={} weight={} tls={}",
            self.host,
            self.port,
            match self.ops_port {
                Some(port) => format!("{}:{port}", self.ops_host),
                None => "shared".to_owned(),
            },
            self.max_connections,
            self.max_sessions,
            self.max_connections_per_session,
            self.ingress_budget_bytes,
            self.ingress_small_reserve_bytes,
            self.ingress_weight,
            self.tls_cert.is_some()
        )
    }
}

/// Every `PASEO_RELAY_*` variable the relay or its tooling understands.
const KNOWN_VARS: &[&str] = &[
    "PASEO_RELAY_HOST",
    "PASEO_RELAY_PORT",
    "PASEO_RELAY_OPS_HOST",
    "PASEO_RELAY_OPS_PORT",
    "PASEO_RELAY_DRAIN",
    "PASEO_RELAY_MAX_CONNECTIONS",
    "PASEO_RELAY_MAX_SESSIONS",
    "PASEO_RELAY_MAX_TCP_CONNECTIONS",
    "PASEO_RELAY_MAX_CLIENTS_PER_CONNECTION",
    "PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION",
    "PASEO_RELAY_INGRESS_BUDGET_BYTES",
    "PASEO_RELAY_INGRESS_SMALL_RESERVE_BYTES",
    "PASEO_RELAY_INGRESS_RECLAIM_MIN_RATE_BYTES",
    "PASEO_RELAY_INGRESS_RECLAIM_AFTER_MS",
    "PASEO_RELAY_INGRESS_WEIGHT",
    "PASEO_RELAY_INGRESS_WAIT_TIMEOUT_MS",
    "PASEO_RELAY_MAX_MESSAGE_BYTES",
    "PASEO_RELAY_CONTROL_QUEUE_BYTES",
    "PASEO_RELAY_DATA_ATTACH_TIMEOUT_MS",
    "PASEO_RELAY_DELIVERY_TIMEOUT_MS",
    "PASEO_RELAY_CONTROL_NUDGE_MS",
    "PASEO_RELAY_CONTROL_UNRESPONSIVE_MS",
    "PASEO_RELAY_FRAME_READ_TIMEOUT_MS",
    "PASEO_RELAY_MESSAGE_READ_TIMEOUT_MS",
    "PASEO_RELAY_READ_STALL_TIMEOUT_MS",
    "PASEO_RELAY_WS_IDLE_TIMEOUT_MS",
    "PASEO_RELAY_SESSION_IDLE_MS",
    "PASEO_RELAY_PER_IP_MAX_CONNECTIONS",
    "PASEO_RELAY_TCP_KEEPALIVE_MS",
    "PASEO_RELAY_SHUTDOWN_GRACE_MS",
    "PASEO_RELAY_HTTP_HEADER_TIMEOUT_MS",
    "PASEO_RELAY_TLS_CERT",
    "PASEO_RELAY_TLS_KEY",
    "PASEO_RELAY_ALLOWED_ORIGINS",
    "PASEO_RELAY_LOG",
    "PASEO_RELAY_LOG_FORMAT",
    // Read by the `healthcheck` subcommand, the build script, and the
    // Compose/CI tooling rather than by `Config`.
    "PASEO_RELAY_HEALTHCHECK_HOST",
    "PASEO_RELAY_BUILD_REVISION",
    "PASEO_RELAY_IMAGE",
    "PASEO_RELAY_MEM_LIMIT",
    "PASEO_RELAY_CI_PORT",
    "PASEO_RELAY_E2E_PORT",
];

/// `PASEO_RELAY_*` variables that are set but not understood, sorted.
///
/// Configuration is environment-only, so a misspelled name is otherwise
/// silently ignored and the setting it was meant to change keeps its default.
pub fn unknown_vars<I, K>(names: I) -> Vec<String>
where
    I: IntoIterator<Item = K>,
    K: AsRef<str>,
{
    let mut unknown: Vec<String> = names
        .into_iter()
        .map(|name| name.as_ref().to_owned())
        .filter(|name| {
            name.starts_with("PASEO_RELAY_")
                && !KNOWN_VARS.contains(&name.as_str())
                && !is_service_link(name)
        })
        .collect();
    unknown.sort();
    unknown.dedup();
    unknown
}

/// Whether `name` has the shape of a Kubernetes service-link variable.
///
/// For every Service in the namespace the kubelet injects
/// `<SVC>_SERVICE_HOST`, `<SVC>_SERVICE_PORT[_<NAME>]`, `<SVC>_PORT` and
/// `<SVC>_PORT_<N>_<PROTO>[_PROTO|_PORT|_ADDR]`. A Service called
/// `paseo-relay` (or `paseo-relay-anything`) makes those start with
/// `PASEO_RELAY_`; they are not misspelled settings.
fn is_service_link(name: &str) -> bool {
    if name.ends_with("_SERVICE_HOST") || name.contains("_SERVICE_PORT") {
        return true;
    }
    // `..._PORT_<digits>_<TCP|UDP|SCTP>...`
    name.match_indices("_PORT_").any(|(index, marker)| {
        let rest = &name[index + marker.len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        digits > 0
            && ["_TCP", "_UDP", "_SCTP"]
                .iter()
                .any(|proto| rest[digits..].starts_with(proto))
    })
}

/// Spare descriptors the process needs beyond client sockets (listeners,
/// epoll, logs, certificate reloads, operations connections).
const FD_HEADROOM: u64 = 128;

/// Soft `RLIMIT_NOFILE` of this process, when the platform exposes it.
pub fn open_file_limit() -> Option<u64> {
    let limits = std::fs::read_to_string("/proc/self/limits").ok()?;
    parse_open_file_limit(&limits)
}

fn parse_open_file_limit(limits: &str) -> Option<u64> {
    let line = limits
        .lines()
        .find(|line| line.starts_with("Max open files"))?;
    let soft = line["Max open files".len()..].split_whitespace().next()?;
    if soft == "unlimited" {
        Some(u64::MAX)
    } else {
        soft.parse().ok()
    }
}

impl Config {
    /// A hint when the relay looks directly exposed but has no per-IP cap.
    ///
    /// The cap cannot be on by default: behind a proxy every client shares
    /// the proxy's address. With native TLS there is usually no such proxy,
    /// and without a cap one client may take every WebSocket slot.
    pub fn per_ip_hint(&self) -> Option<String> {
        (self.tls_cert.is_some() && self.per_ip_max_connections == 0).then(|| {
            "native TLS is configured, so clients probably connect directly, but \
             PASEO_RELAY_PER_IP_MAX_CONNECTIONS is 0 (unlimited); set it (for example 256) \
             unless a proxy in front shares one address for all clients"
                .to_owned()
        })
    }

    /// A warning when the open-file limit cannot hold the configured sockets.
    ///
    /// Every accepted socket is one descriptor. If the soft limit is below
    /// `PASEO_RELAY_MAX_TCP_CONNECTIONS`, `accept` starts failing long before
    /// the relay's own caps are reached and nothing in `/ready` says why.
    pub fn open_file_limit_warning(&self, limit: Option<u64>) -> Option<String> {
        let limit = limit?;
        let needed = (self.max_tcp_connections as u64).saturating_add(FD_HEADROOM);
        (limit < needed).then(|| {
            format!(
                "open-file limit {limit} is below PASEO_RELAY_MAX_TCP_CONNECTIONS + {FD_HEADROOM} = {needed}; \
                 raise it (ulimit -n / LimitNOFILE / container nofile) or lower the connection caps"
            )
        })
    }
}

fn get(vars: &HashMap<String, String>, key: &str) -> Option<String> {
    vars.get(key)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn check_readable_file(path: &std::path::Path, key: &str) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("{key} must point to a readable file: {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{key} must point to a regular file: {}", path.display());
    }
    std::fs::File::open(path)
        .with_context(|| format!("{key} must be readable: {}", path.display()))?;
    Ok(())
}

fn parse_u64(key: &str, value: &str, min: u64, max: u64) -> Result<u64> {
    let parsed: u64 = value
        .parse()
        .with_context(|| format!("{key} must be an integer between {min} and {max}"))?;
    if parsed < min || parsed > max {
        bail!("{key} must be an integer between {min} and {max}");
    }
    Ok(parsed)
}

fn parse_usize(key: &str, value: &str, min: usize, max: usize) -> Result<usize> {
    let parsed: usize = value
        .parse()
        .with_context(|| format!("{key} must be an integer between {min} and {max}"))?;
    if parsed < min || parsed > max {
        bail!("{key} must be an integer between {min} and {max}");
    }
    Ok(parsed)
}

fn parse_bool(key: &str, value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("{key} must be true or false"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(vars: &[(&str, &str)]) -> HashMap<String, String> {
        vars.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn max_message_charge_includes_chunk_bookkeeping() {
        let cfg = Config::default();
        assert_eq!(
            cfg.max_message_weighted_charge(),
            (cfg.max_message_bytes as u64 + FIXED_MESSAGE_OVERHEAD) * cfg.ingress_weight as u64
                + (cfg.max_message_bytes.div_ceil(INGRESS_CHUNK_BYTES) + MAX_FRAGMENTS_PER_MESSAGE)
                    as u64
                    * INGRESS_CHUNK_OVERHEAD
        );
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn compose_129mib_budget_is_accepted_and_128mib_is_not() {
        let accepted = Config {
            ingress_budget_bytes: 135_266_304,
            ..Config::default()
        };
        assert!(accepted.validate().is_ok());

        let rejected = Config {
            ingress_budget_bytes: 128 * 1024 * 1024,
            ..Config::default()
        };
        assert!(rejected.validate().is_err());
    }

    #[test]
    fn small_reserve_must_admit_one_small_message_or_be_disabled() {
        let cfg = Config::default();
        let floor = cfg.small_message_weighted_charge();
        assert!(floor < cfg.ingress_small_reserve_bytes);

        let disabled = Config {
            ingress_small_reserve_bytes: 0,
            ..Config::default()
        };
        assert!(disabled.validate().is_ok());

        let exact = Config {
            ingress_small_reserve_bytes: floor,
            ..Config::default()
        };
        assert!(exact.validate().is_ok());

        let too_small = Config {
            ingress_small_reserve_bytes: floor - 1,
            ..Config::default()
        };
        assert!(too_small.validate().is_err());
    }

    #[test]
    fn reads_new_ingress_and_timeout_settings() {
        let cfg = Config::from_map(&map(&[
            ("PASEO_RELAY_INGRESS_SMALL_RESERVE_BYTES", "33554432"),
            ("PASEO_RELAY_INGRESS_RECLAIM_MIN_RATE_BYTES", "262144"),
            ("PASEO_RELAY_INGRESS_RECLAIM_AFTER_MS", "120000"),
            ("PASEO_RELAY_READ_STALL_TIMEOUT_MS", "5000"),
            ("PASEO_RELAY_WS_IDLE_TIMEOUT_MS", "600000"),
        ]))
        .unwrap();
        assert_eq!(cfg.ingress_small_reserve_bytes, 32 * 1024 * 1024);
        assert_eq!(cfg.ingress_reclaim_min_rate_bytes, 256 * 1024);
        assert_eq!(cfg.ingress_reclaim_after, Duration::from_secs(120));
        // 0 disables the age rule; anything else below 5 s is refused.
        assert!(Config::from_map(&map(&[("PASEO_RELAY_INGRESS_RECLAIM_AFTER_MS", "0")])).is_ok());
        assert!(
            Config::from_map(&map(&[("PASEO_RELAY_INGRESS_RECLAIM_AFTER_MS", "1000")])).is_err()
        );
        assert_eq!(cfg.read_stall_timeout, Duration::from_secs(5));
        assert_eq!(cfg.ws_idle_timeout, Duration::from_secs(600));

        // The idle timeout is off by default and rejects sub-second values.
        assert!(Config::default().ws_idle_timeout.is_zero());
        assert!(Config::from_map(&map(&[("PASEO_RELAY_WS_IDLE_TIMEOUT_MS", "500")])).is_err());
    }

    #[test]
    fn ops_listener_is_optional_and_must_not_collide() {
        assert_eq!(Config::default().ops_port, None);

        let cfg = Config::from_map(&map(&[
            ("PASEO_RELAY_OPS_HOST", "0.0.0.0"),
            ("PASEO_RELAY_OPS_PORT", "4001"),
        ]))
        .unwrap();
        assert_eq!(cfg.ops_port, Some(4001));
        assert_eq!(cfg.ops_host, "0.0.0.0".parse::<IpAddr>().unwrap());

        let err = Config::from_map(&map(&[("PASEO_RELAY_OPS_PORT", "4000")])).unwrap_err();
        assert!(err.to_string().contains("PASEO_RELAY_OPS_PORT"));

        // An unspecified address (0.0.0.0) overlaps loopback on the same port
        // on Linux and fails at bind time; reject it at validate time.
        assert!(Config::from_map(&map(&[
            ("PASEO_RELAY_HOST", "0.0.0.0"),
            ("PASEO_RELAY_OPS_PORT", "4000"),
        ]))
        .is_err());

        // Distinct non-unspecified addresses on the same port do not overlap.
        assert!(Config::from_map(&map(&[
            ("PASEO_RELAY_HOST", "10.0.0.10"),
            ("PASEO_RELAY_OPS_HOST", "127.0.0.1"),
            ("PASEO_RELAY_OPS_PORT", "4000"),
        ]))
        .is_ok());

        let cfg =
            Config::from_map(&map(&[("PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION", "128")])).unwrap();
        assert_eq!(cfg.max_connections_per_session, 128);
        assert!(
            Config::from_map(&map(&[("PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION", "0",)])).is_err()
        );
    }

    #[test]
    fn unknown_relay_variables_are_reported() {
        let unknown = unknown_vars([
            "PASEO_RELAY_PORT",
            "PASEO_RELAY_PER_IP_MAX_CONNECTION",
            "PASEO_RELAY_HEALTHCHECK_HOST",
            "PATH",
            "PASEO_RELAY_MAX_SESSION",
        ]);
        assert_eq!(
            unknown,
            vec![
                "PASEO_RELAY_MAX_SESSION".to_owned(),
                "PASEO_RELAY_PER_IP_MAX_CONNECTION".to_owned(),
            ]
        );
    }

    #[test]
    fn kubernetes_service_links_are_not_reported_as_misspellings() {
        // What the kubelet injects for a Service named `paseo-relay` with a
        // port called `http`, and for one named `paseo-relay-ops`.
        let injected = [
            "PASEO_RELAY_SERVICE_HOST",
            "PASEO_RELAY_SERVICE_PORT",
            "PASEO_RELAY_SERVICE_PORT_HTTP",
            "PASEO_RELAY_PORT_4000_TCP",
            "PASEO_RELAY_PORT_4000_TCP_PROTO",
            "PASEO_RELAY_PORT_4000_TCP_PORT",
            "PASEO_RELAY_PORT_4000_TCP_ADDR",
            "PASEO_RELAY_OPS_SERVICE_HOST",
            "PASEO_RELAY_OPS_PORT_4001_TCP_ADDR",
        ];
        assert!(unknown_vars(injected).is_empty());
        // A real typo that merely contains PORT is still reported.
        assert_eq!(
            unknown_vars(["PASEO_RELAY_OPS_PORTS", "PASEO_RELAY_PORT_NUMBER"]),
            vec![
                "PASEO_RELAY_OPS_PORTS".to_owned(),
                "PASEO_RELAY_PORT_NUMBER".to_owned()
            ]
        );
    }

    #[test]
    fn service_link_port_value_gets_an_actionable_error() {
        let err =
            Config::from_map(&map(&[("PASEO_RELAY_PORT", "tcp://10.96.12.7:4000")])).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("enableServiceLinks") && message.contains("service link"),
            "{message}"
        );
    }

    #[test]
    fn every_parsed_variable_is_a_known_variable() {
        // Guards against adding a setting to `from_map` without listing it.
        let source = include_str!("config.rs");
        for (index, _) in source.match_indices("get(vars, \"PASEO_RELAY_") {
            let rest = &source[index + "get(vars, \"".len()..];
            let name = &rest[..rest.find('"').unwrap()];
            assert!(KNOWN_VARS.contains(&name), "{name} missing from KNOWN_VARS");
        }
    }

    #[test]
    fn parses_the_soft_open_file_limit() {
        let limits = "Limit                     Soft Limit           Hard Limit           Units     \n\
                      Max cpu time              unlimited            unlimited            seconds   \n\
                      Max open files            1024                 524288               files     \n";
        assert_eq!(parse_open_file_limit(limits), Some(1024));
        assert_eq!(
            parse_open_file_limit(
                "Max open files            unlimited            unlimited            files"
            ),
            Some(u64::MAX)
        );
        assert_eq!(parse_open_file_limit("nothing here"), None);
    }

    #[test]
    fn hints_at_a_per_ip_cap_only_for_native_tls_without_one() {
        let tls = Config {
            tls_cert: Some(PathBuf::from("/tls/cert.pem")),
            tls_key: Some(PathBuf::from("/tls/key.pem")),
            ..Config::default()
        };
        assert!(tls.per_ip_hint().is_some());
        let capped = Config {
            per_ip_max_connections: 256,
            ..tls
        };
        assert!(capped.per_ip_hint().is_none());
        // Plaintext means an edge proxy terminates TLS: every client shares
        // its address, so a cap would be wrong.
        assert!(Config::default().per_ip_hint().is_none());
    }

    #[test]
    fn warns_when_the_open_file_limit_cannot_hold_the_tcp_cap() {
        let cfg = Config::default();
        let warning = cfg.open_file_limit_warning(Some(1024)).unwrap();
        assert!(
            warning.contains("1024") && warning.contains("25128"),
            "{warning}"
        );
        assert!(cfg.open_file_limit_warning(Some(65_536)).is_none());
        assert!(cfg.open_file_limit_warning(None).is_none());
    }

    #[test]
    fn rejects_low_ingress_weight() {
        assert!(
            Config::from_map(&map(&[("PASEO_RELAY_INGRESS_WEIGHT", "2")])).is_err(),
            "weight below 4 must be rejected"
        );
        assert!(Config::from_map(&map(&[("PASEO_RELAY_INGRESS_WEIGHT", "4")])).is_ok());
    }

    #[test]
    fn rejects_invalid_log_filter() {
        assert!(
            Config::from_map(&map(&[("PASEO_RELAY_LOG", "not a [ valid filter")])).is_err(),
            "an invalid EnvFilter directive must abort validation"
        );
    }

    #[test]
    fn rejects_directory_as_tls_file() {
        let dir = std::env::temp_dir();
        let unique = format!("paseo-tls-dir-{}", std::process::id());
        let cfg = Config {
            tls_cert: Some(dir.join(&unique)),
            tls_key: Some(dir.join(format!("{unique}-key"))),
            ..Config::default()
        };
        // Create the "cert" path as a directory to prove only regular files pass.
        let cert_dir = cfg.tls_cert.as_ref().unwrap();
        std::fs::create_dir_all(cert_dir).unwrap();
        assert!(cfg.validate().is_err(), "a directory is not a TLS file");
        let _ = std::fs::remove_dir(cert_dir);
    }

    #[test]
    fn rejects_unreadable_tls_files() {
        let cfg = Config {
            tls_cert: Some(PathBuf::from("/nonexistent/paseo-cert.pem")),
            tls_key: Some(PathBuf::from("/nonexistent/paseo-key.pem")),
            ..Config::default()
        };
        assert!(
            cfg.validate().is_err(),
            "missing TLS files must be rejected"
        );
    }

    #[test]
    fn reads_log_format() {
        let cfg = Config::from_map(&map(&[("PASEO_RELAY_LOG_FORMAT", "json")])).unwrap();
        assert!(cfg.log_json);
        assert!(Config::from_map(&map(&[("PASEO_RELAY_LOG_FORMAT", "yaml")])).is_err());
    }

    #[test]
    fn reads_frame_read_timeouts() {
        let cfg = Config::from_map(&map(&[
            ("PASEO_RELAY_FRAME_READ_TIMEOUT_MS", "250"),
            ("PASEO_RELAY_MESSAGE_READ_TIMEOUT_MS", "1500"),
        ]))
        .unwrap();
        assert_eq!(cfg.frame_read_timeout, Duration::from_millis(250));
        assert_eq!(cfg.message_read_timeout, Duration::from_millis(1500));
    }

    #[test]
    fn parses_custom_settings() {
        let cfg = Config::from_map(&map(&[
            ("PASEO_RELAY_HOST", "0.0.0.0"),
            ("PASEO_RELAY_PORT", "4100"),
            ("PASEO_RELAY_MAX_CONNECTIONS", "100"),
            ("PASEO_RELAY_MAX_SESSIONS", "50"),
            ("PASEO_RELAY_INGRESS_WEIGHT", "8"),
            (
                "PASEO_RELAY_ALLOWED_ORIGINS",
                "https://app.paseo.sh, https://x.test",
            ),
        ]))
        .unwrap();
        assert_eq!(cfg.port, 4100);
        assert_eq!(cfg.max_connections, 100);
        assert_eq!(cfg.max_sessions, 50);
        assert_eq!(cfg.ingress_weight, 8);
        assert_eq!(
            cfg.allowed_origins,
            vec!["https://app.paseo.sh", "https://x.test"]
        );
    }

    #[test]
    fn rejects_budget_below_one_message() {
        let err = Config::from_map(&map(&[
            ("PASEO_RELAY_INGRESS_BUDGET_BYTES", "134217728"),
            ("PASEO_RELAY_INGRESS_WEIGHT", "8"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("one maximum message"));
    }

    #[test]
    fn rejects_half_tls_config() {
        let err = Config::from_map(&map(&[("PASEO_RELAY_TLS_CERT", "/tmp/cert.pem")])).unwrap_err();
        assert!(err.to_string().contains("TLS_CERT"));
    }

    #[test]
    fn rejects_max_connections_above_tcp_limit() {
        let err = Config::from_map(&map(&[
            ("PASEO_RELAY_MAX_CONNECTIONS", "20000"),
            ("PASEO_RELAY_MAX_TCP_CONNECTIONS", "10000"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("MAX_TCP_CONNECTIONS"));
    }
}
