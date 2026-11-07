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

#[derive(Debug, Clone)]
pub struct Config {
    pub host: IpAddr,
    pub port: u16,
    pub drain: bool,

    /// Maximum number of upgraded WebSockets.
    pub max_connections: usize,
    /// Maximum number of live sessions (`serverId` entries).
    pub max_sessions: usize,
    /// Maximum number of open TCP connections (pre-upgrade HTTP included).
    pub max_tcp_connections: usize,
    /// Maximum number of client sockets attached to one v2 connectionId.
    pub max_clients_per_connection: usize,

    /// Weighted byte budget for admitted, in-flight WebSocket payloads.
    pub ingress_budget_bytes: u64,
    /// Conservative memory weight charged per payload byte.
    pub ingress_weight: u32,
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
            drain: false,
            max_connections: 20_000,
            max_sessions: 20_000,
            max_tcp_connections: 25_000,
            max_clients_per_connection: 32,
            ingress_budget_bytes: 512 * 1024 * 1024,
            ingress_weight: 4,
            ingress_wait_timeout: Duration::from_millis(5_000),
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            control_queue_bytes: 1024 * 1024,
            data_attach_timeout: Duration::from_millis(15_000),
            control_nudge_after: Duration::from_millis(10_000),
            control_unresponsive_after: Duration::from_millis(15_000),
            delivery_timeout: Duration::from_millis(30_000),
            frame_read_timeout: Duration::from_millis(300_000),
            message_read_timeout: Duration::from_millis(600_000),
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
            cfg.port = parse_u64("PASEO_RELAY_PORT", &value, 1, 65_535)? as u16;
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
        if let Some(value) = get(vars, "PASEO_RELAY_INGRESS_BUDGET_BYTES") {
            cfg.ingress_budget_bytes = parse_u64(
                "PASEO_RELAY_INGRESS_BUDGET_BYTES",
                &value,
                128 * 1024 * 1024,
                4 * 1024 * 1024 * 1024,
            )?;
        }
        if let Some(value) = get(vars, "PASEO_RELAY_INGRESS_WEIGHT") {
            cfg.ingress_weight = parse_u64("PASEO_RELAY_INGRESS_WEIGHT", &value, 1, 16)? as u32;
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
        let one_max_message = (self.max_message_bytes as u64 + FIXED_MESSAGE_OVERHEAD)
            .checked_mul(self.ingress_weight as u64)
            .context("ingress budget weight overflow")?;
        if self.ingress_budget_bytes < one_max_message {
            bail!(
                "PASEO_RELAY_INGRESS_BUDGET_BYTES must admit one maximum message at the configured weight"
            );
        }
        match (&self.tls_cert, &self.tls_key) {
            (Some(_), Some(_)) | (None, None) => {}
            _ => bail!("PASEO_RELAY_TLS_CERT and PASEO_RELAY_TLS_KEY must be set together"),
        }
        if self.max_clients_per_connection == 0 {
            bail!("PASEO_RELAY_MAX_CLIENTS_PER_CONNECTION must be >= 1");
        }
        if self.control_unresponsive_after <= self.control_nudge_after {
            bail!("PASEO_RELAY_CONTROL_UNRESPONSIVE_MS must be greater than PASEO_RELAY_CONTROL_NUDGE_MS");
        }
        if self.control_queue_bytes == 0 {
            bail!("PASEO_RELAY_CONTROL_QUEUE_BYTES must be > 0");
        }
        Ok(())
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
            "{}:{} max_connections={} max_sessions={} ingress_budget={} weight={} tls={}",
            self.host,
            self.port,
            self.max_connections,
            self.max_sessions,
            self.ingress_budget_bytes,
            self.ingress_weight,
            self.tls_cert.is_some()
        )
    }
}

fn get(vars: &HashMap<String, String>, key: &str) -> Option<String> {
    vars.get(key)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
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
