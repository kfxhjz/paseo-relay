//! Lock-free counters, the Prometheus text surface, and rate-limited
//! operator warnings for shed events.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// A reason the relay turned work away or closed a connection to protect
/// itself. Each kind is counted and logged at most once every ten seconds, so
/// an operator without a metrics pipeline still sees that the node is under
/// pressure, without one log line per event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShedKind {
    IngressExhausted,
    IngressReclaimed,
    ReadTimeout,
    IdleTimeout,
    SlowConsumer,
    WebsocketCapacity,
    SessionCapacity,
    TcpCapacity,
    PerIpLimit,
}

const SHED_KINDS: usize = 9;

/// Minimum spacing between two warn lines of the same [`ShedKind`].
const SHED_LOG_INTERVAL_MS: u64 = 10_000;

impl ShedKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ShedKind::IngressExhausted => "ingress_exhausted",
            ShedKind::IngressReclaimed => "ingress_reclaimed",
            ShedKind::ReadTimeout => "read_timeout",
            ShedKind::IdleTimeout => "idle_timeout",
            ShedKind::SlowConsumer => "slow_consumer",
            ShedKind::WebsocketCapacity => "websocket_capacity",
            ShedKind::SessionCapacity => "session_capacity",
            ShedKind::TcpCapacity => "tcp_capacity",
            ShedKind::PerIpLimit => "per_ip_limit",
        }
    }

    fn message(self) -> &'static str {
        match self {
            ShedKind::IngressExhausted => {
                "ingress budget exhausted: closing sources that could not reserve in time"
            }
            ShedKind::IngressReclaimed => {
                "ingress budget under pressure: reclaiming stalled or oversized in-progress messages"
            }
            ShedKind::ReadTimeout => "closing sources whose message stalled or timed out",
            ShedKind::IdleTimeout => "closing idle websockets",
            ShedKind::SlowConsumer => "shedding slow destinations",
            ShedKind::WebsocketCapacity => "rejecting upgrades: websocket capacity reached",
            ShedKind::SessionCapacity => "rejecting new sessions: session capacity reached",
            ShedKind::TcpCapacity => "rejecting connections: TCP capacity reached",
            ShedKind::PerIpLimit => "rejecting connections: per-IP limit reached",
        }
    }
}

#[derive(Debug, Default)]
struct ShedSlot {
    /// Milliseconds since start (plus one) of the last warn line; 0 = never.
    last_log_ms: AtomicU64,
    /// Events since the last warn line.
    pending: AtomicU64,
}

#[derive(Debug)]
pub struct Metrics {
    pub frames_forwarded: AtomicU64,
    pub bytes_forwarded: AtomicU64,
    pub slow_consumer_disconnects: AtomicU64,
    pub delivery_timeouts: AtomicU64,
    pub delivery_ack_timeouts: AtomicU64,
    pub control_send_failures: AtomicU64,
    pub connection_rejections: AtomicU64,
    pub session_rejections: AtomicU64,
    pub session_connection_rejections: AtomicU64,
    pub client_limit_rejections: AtomicU64,
    pub ops_rejections: AtomicU64,
    pub per_ip_rejections: AtomicU64,
    pub tcp_rejections: AtomicU64,
    pub ingress_rejections: AtomicU64,
    pub ingress_reclaims: AtomicU64,
    pub read_timeouts: AtomicU64,
    pub idle_timeouts: AtomicU64,
    pub handshake_accepted: AtomicU64,
    pub handshake_rejected: AtomicU64,
    pub max_frame_bytes: AtomicU64,
    pub started: Instant,
    shed: [ShedSlot; SHED_KINDS],
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Point-in-time values rendered next to the counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct Gauges {
    pub active_websockets: u64,
    pub active_sessions: u64,
    pub ingress_reserved_bytes: u64,
    pub ingress_budget_bytes: u64,
    pub ingress_small_reserved_bytes: u64,
    pub ingress_small_budget_bytes: u64,
    pub ready: bool,
    pub draining: bool,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            frames_forwarded: AtomicU64::new(0),
            bytes_forwarded: AtomicU64::new(0),
            slow_consumer_disconnects: AtomicU64::new(0),
            delivery_timeouts: AtomicU64::new(0),
            delivery_ack_timeouts: AtomicU64::new(0),
            control_send_failures: AtomicU64::new(0),
            connection_rejections: AtomicU64::new(0),
            session_rejections: AtomicU64::new(0),
            session_connection_rejections: AtomicU64::new(0),
            client_limit_rejections: AtomicU64::new(0),
            ops_rejections: AtomicU64::new(0),
            per_ip_rejections: AtomicU64::new(0),
            tcp_rejections: AtomicU64::new(0),
            ingress_rejections: AtomicU64::new(0),
            ingress_reclaims: AtomicU64::new(0),
            read_timeouts: AtomicU64::new(0),
            idle_timeouts: AtomicU64::new(0),
            handshake_accepted: AtomicU64::new(0),
            handshake_rejected: AtomicU64::new(0),
            max_frame_bytes: AtomicU64::new(0),
            started: Instant::now(),
            shed: Default::default(),
        }
    }

    pub fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add(counter: &AtomicU64, amount: u64) {
        counter.fetch_add(amount, Ordering::Relaxed);
    }

    pub fn observe_frame_size(&self, bytes: usize) {
        let bytes = bytes as u64;
        let _ = self.max_frame_bytes.fetch_max(bytes, Ordering::Relaxed);
    }

    /// Note one shed event and warn about its kind at most once per interval.
    ///
    /// The first event of a kind is logged immediately; later ones are
    /// counted and reported with the next line, so a quiet period after a
    /// burst leaves the tail of the burst only in the counters.
    pub fn shed(&self, kind: ShedKind) {
        if let Some(count) = self.shed_due(kind, self.started.elapsed().as_millis() as u64) {
            tracing::warn!(event = kind.as_str(), count, "{}", kind.message());
        }
    }

    fn shed_due(&self, kind: ShedKind, elapsed_ms: u64) -> Option<u64> {
        let slot = &self.shed[kind as usize];
        slot.pending.fetch_add(1, Ordering::Relaxed);
        let now = elapsed_ms.saturating_add(1);
        let last = slot.last_log_ms.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < SHED_LOG_INTERVAL_MS {
            return None;
        }
        slot.last_log_ms
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .ok()?;
        Some(slot.pending.swap(0, Ordering::Relaxed))
    }

    pub fn render(&self, gauges: Gauges) -> String {
        let mut out = String::with_capacity(4096);
        let started = self.started.elapsed().as_secs_f64();

        metric(
            &mut out,
            "paseo_relay_ready",
            "gauge",
            "Whether this node admits new relay work.",
            u8::from(gauges.ready),
        );
        metric(
            &mut out,
            "paseo_relay_draining",
            "gauge",
            "Whether this node is draining.",
            u8::from(gauges.draining),
        );
        metric(
            &mut out,
            "paseo_relay_active_websockets",
            "gauge",
            "Open WebSocket connections on this node.",
            gauges.active_websockets,
        );
        metric(
            &mut out,
            "paseo_relay_active_sessions",
            "gauge",
            "Relay sessions owned by this node.",
            gauges.active_sessions,
        );
        metric(
            &mut out,
            "paseo_relay_ingress_reserved_bytes",
            "gauge",
            "Weighted ingress bytes currently reserved.",
            gauges.ingress_reserved_bytes,
        );
        metric(
            &mut out,
            "paseo_relay_ingress_budget_bytes",
            "gauge",
            "Configured weighted ingress budget for this node.",
            gauges.ingress_budget_bytes,
        );
        metric(
            &mut out,
            "paseo_relay_ingress_small_reserved_bytes",
            "gauge",
            "Weighted ingress bytes currently reserved from the small-message reserve.",
            gauges.ingress_small_reserved_bytes,
        );
        metric(
            &mut out,
            "paseo_relay_ingress_small_budget_bytes",
            "gauge",
            "Configured weighted small-message reserve for this node.",
            gauges.ingress_small_budget_bytes,
        );
        counter(
            &mut out,
            "paseo_relay_frames_forwarded_total",
            "Relayed WebSocket user frames forwarded by this node (control frames such as sync/connected/disconnected/pong are excluded).",
            &self.frames_forwarded,
        );
        counter(
            &mut out,
            "paseo_relay_bytes_forwarded_total",
            "Relayed WebSocket user payload bytes forwarded by this node (control-frame bytes are excluded).",
            &self.bytes_forwarded,
        );
        counter(
            &mut out,
            "paseo_relay_slow_consumer_disconnects_total",
            "Destinations disconnected after exceeding a delivery deadline.",
            &self.slow_consumer_disconnects,
        );
        counter(
            &mut out,
            "paseo_relay_delivery_timeouts_total",
            "Shed deliveries (relayed user frames or relay-generated control notifications) that exceeded their deadline (counted once per shed: source for queue waits, writer for write deadlines).",
            &self.delivery_timeouts,
        );
        counter(
            &mut out,
            "paseo_relay_delivery_ack_timeouts_total",
            "Source-side acknowledgement waits that expired at the delivery deadline. Overlaps with writer-counted delivery_timeouts_total; a write completed exactly at the boundary still closes the destination as a slow consumer, so this is not a per-shed count.",
            &self.delivery_ack_timeouts,
        );
        counter(
            &mut out,
            "paseo_relay_control_send_failures_total",
            "Control notifications (sync/connected/disconnected/pong) that could not be queued or written to a control socket.",
            &self.control_send_failures,
        );
        counter(
            &mut out,
            "paseo_relay_connection_rejections_total",
            "WebSocket upgrades rejected at capacity, while draining, or for invalid handshake/Origin/upgrade failures.",
            &self.connection_rejections,
        );
        counter(
            &mut out,
            "paseo_relay_session_rejections_total",
            "Sessions rejected at the configured global session ceiling (MAX_SESSIONS).",
            &self.session_rejections,
        );
        counter(
            &mut out,
            "paseo_relay_session_connection_rejections_total",
            "Sockets rejected at the per-session connection ceiling or because the session sync frame would exceed the control-frame limit.",
            &self.session_connection_rejections,
        );
        counter(
            &mut out,
            "paseo_relay_client_limit_rejections_total",
            "Clients rejected at the per-connectionId client ceiling.",
            &self.client_limit_rejections,
        );
        counter(
            &mut out,
            "paseo_relay_ops_rejections_total",
            "Operations connections rejected by the operations-listener connection allowance.",
            &self.ops_rejections,
        );
        counter(
            &mut out,
            "paseo_relay_per_ip_rejections_total",
            "WebSocket upgrades rejected by the per-IP connection limit.",
            &self.per_ip_rejections,
        );
        counter(
            &mut out,
            "paseo_relay_tcp_rejections_total",
            "Accepted sockets rejected at TCP capacity or by the per-IP TCP limit.",
            &self.tcp_rejections,
        );
        counter(
            &mut out,
            "paseo_relay_ingress_rejections_total",
            "Sources closed because an ingress budget reservation timed out.",
            &self.ingress_rejections,
        );
        counter(
            &mut out,
            "paseo_relay_ingress_reclaims_total",
            "In-progress messages reclaimed to free ingress budget for a waiting source.",
            &self.ingress_reclaims,
        );
        counter(
            &mut out,
            "paseo_relay_read_timeouts_total",
            "Sources closed because a frame or message stalled or exceeded its read deadline.",
            &self.read_timeouts,
        );
        counter(
            &mut out,
            "paseo_relay_idle_timeouts_total",
            "WebSockets closed by the optional idle timeout.",
            &self.idle_timeouts,
        );
        counter(
            &mut out,
            "paseo_relay_handshake_accepted_total",
            "Client E2EE handshake frames accepted by the input validator.",
            &self.handshake_accepted,
        );
        counter(
            &mut out,
            "paseo_relay_handshake_rejected_total",
            "Client E2EE handshake frames rejected by the input validator.",
            &self.handshake_rejected,
        );
        metric(
            &mut out,
            "paseo_relay_max_frame_bytes",
            "gauge",
            "Largest WebSocket frame payload observed since start.",
            self.max_frame_bytes.load(Ordering::Relaxed),
        );
        let _ = writeln!(
            out,
            "# HELP paseo_relay_uptime_seconds Relay process uptime.\n\
             # TYPE paseo_relay_uptime_seconds gauge\n\
             paseo_relay_uptime_seconds {started:.3}"
        );
        let _ = writeln!(
            out,
            "# HELP paseo_relay_build_info Relay build metadata.\n\
             # TYPE paseo_relay_build_info gauge\n\
             paseo_relay_build_info{{version=\"{}\",revision=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION"),
            env!("PASEO_RELAY_BUILD_REVISION")
        );
        out
    }
}

fn counter(out: &mut String, name: &str, help: &str, value: &AtomicU64) {
    let _ = writeln!(
        out,
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {}",
        value.load(Ordering::Relaxed)
    );
}

fn metric(out: &mut String, name: &str, kind: &str, help: &str, value: impl std::fmt::Display) {
    let _ = writeln!(
        out,
        "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_prometheus_lines() {
        let metrics = Metrics::new();
        Metrics::inc(&metrics.handshake_rejected);
        Metrics::inc(&metrics.ingress_reclaims);
        let text = metrics.render(Gauges {
            active_websockets: 2,
            active_sessions: 1,
            ingress_reserved_bytes: 3,
            ingress_budget_bytes: 1024,
            ingress_small_reserved_bytes: 5,
            ingress_small_budget_bytes: 64,
            ready: true,
            draining: false,
        });
        assert!(text.contains("paseo_relay_ready 1"));
        assert!(text.contains("paseo_relay_draining 0"));
        assert!(text.contains("paseo_relay_active_websockets 2"));
        assert!(text.contains("paseo_relay_active_sessions 1"));
        assert!(text.contains("paseo_relay_ingress_reserved_bytes 3"));
        assert!(text.contains("paseo_relay_ingress_budget_bytes 1024"));
        assert!(text.contains("paseo_relay_ingress_small_reserved_bytes 5"));
        assert!(text.contains("paseo_relay_ingress_small_budget_bytes 64"));
        assert!(text.contains("paseo_relay_handshake_rejected_total 1"));
        assert!(text.contains("paseo_relay_ingress_reclaims_total 1"));
        assert!(text.contains("paseo_relay_tcp_rejections_total 0"));
        assert!(text.contains("paseo_relay_session_rejections_total 0"));
        assert!(text.contains("paseo_relay_session_connection_rejections_total 0"));
        assert!(text.contains("paseo_relay_client_limit_rejections_total 0"));
        assert!(text.contains("paseo_relay_ops_rejections_total 0"));
        assert!(text.contains("paseo_relay_control_send_failures_total 0"));
        assert!(text.contains("paseo_relay_delivery_ack_timeouts_total 0"));
        assert!(text.contains("# TYPE paseo_relay_ready gauge"));
    }

    #[test]
    fn shed_warnings_are_rate_limited_per_kind() {
        let metrics = Metrics::new();
        // First event of a kind logs at once.
        assert_eq!(metrics.shed_due(ShedKind::TcpCapacity, 0), Some(1));
        // A burst inside the interval is only counted.
        for ms in 1..500 {
            assert_eq!(metrics.shed_due(ShedKind::TcpCapacity, ms), None);
        }
        // Another kind has its own clock.
        assert_eq!(metrics.shed_due(ShedKind::SlowConsumer, 10), Some(1));
        // The next line after the interval reports the whole burst.
        assert_eq!(
            metrics.shed_due(ShedKind::TcpCapacity, SHED_LOG_INTERVAL_MS + 1),
            Some(500)
        );
    }
}
