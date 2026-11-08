//! Lock-free counters and the Prometheus text surface.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Debug)]
pub struct Metrics {
    pub frames_forwarded: AtomicU64,
    pub bytes_forwarded: AtomicU64,
    pub slow_consumer_disconnects: AtomicU64,
    pub delivery_timeouts: AtomicU64,
    pub connection_rejections: AtomicU64,
    pub session_rejections: AtomicU64,
    pub per_ip_rejections: AtomicU64,
    pub handshake_accepted: AtomicU64,
    pub handshake_rejected: AtomicU64,
    pub max_frame_bytes: AtomicU64,
    pub started: Instant,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            frames_forwarded: AtomicU64::new(0),
            bytes_forwarded: AtomicU64::new(0),
            slow_consumer_disconnects: AtomicU64::new(0),
            delivery_timeouts: AtomicU64::new(0),
            connection_rejections: AtomicU64::new(0),
            session_rejections: AtomicU64::new(0),
            per_ip_rejections: AtomicU64::new(0),
            handshake_accepted: AtomicU64::new(0),
            handshake_rejected: AtomicU64::new(0),
            max_frame_bytes: AtomicU64::new(0),
            started: Instant::now(),
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

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        active_websockets: u64,
        active_sessions: u64,
        ingress_reserved_bytes: u64,
        ingress_budget_bytes: u64,
        ready: bool,
        draining: bool,
    ) -> String {
        let mut out = String::with_capacity(2048);
        let started = self.started.elapsed().as_secs_f64();

        metric(
            &mut out,
            "paseo_relay_ready",
            "gauge",
            "Whether this node admits new relay work.",
            if ready { 1 } else { 0 },
        );
        metric(
            &mut out,
            "paseo_relay_draining",
            "gauge",
            "Whether this node is draining.",
            if draining { 1 } else { 0 },
        );
        metric(
            &mut out,
            "paseo_relay_active_websockets",
            "gauge",
            "Open WebSocket connections on this node.",
            active_websockets,
        );
        metric(
            &mut out,
            "paseo_relay_active_sessions",
            "gauge",
            "Relay sessions owned by this node.",
            active_sessions,
        );
        metric(
            &mut out,
            "paseo_relay_ingress_reserved_bytes",
            "gauge",
            "Weighted ingress bytes currently reserved.",
            ingress_reserved_bytes,
        );
        metric(
            &mut out,
            "paseo_relay_ingress_budget_bytes",
            "gauge",
            "Configured weighted ingress budget for this node.",
            ingress_budget_bytes,
        );
        counter(
            &mut out,
            "paseo_relay_frames_forwarded_total",
            "WebSocket frames forwarded by this node.",
            &self.frames_forwarded,
        );
        counter(
            &mut out,
            "paseo_relay_bytes_forwarded_total",
            "WebSocket payload bytes forwarded by this node.",
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
            "Deliveries that exceeded their deadline.",
            &self.delivery_timeouts,
        );
        counter(
            &mut out,
            "paseo_relay_connection_rejections_total",
            "WebSocket upgrades rejected at configured capacity or while draining.",
            &self.connection_rejections,
        );
        counter(
            &mut out,
            "paseo_relay_session_rejections_total",
            "Sessions rejected at the configured session ceiling.",
            &self.session_rejections,
        );
        counter(
            &mut out,
            "paseo_relay_per_ip_rejections_total",
            "Connections rejected by the per-IP connection limit.",
            &self.per_ip_rejections,
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
        let text = metrics.render(2, 1, 3, 1024, true, false);
        assert!(text.contains("paseo_relay_ready 1"));
        assert!(text.contains("paseo_relay_draining 0"));
        assert!(text.contains("paseo_relay_active_websockets 2"));
        assert!(text.contains("paseo_relay_active_sessions 1"));
        assert!(text.contains("paseo_relay_ingress_reserved_bytes 3"));
        assert!(text.contains("paseo_relay_ingress_budget_bytes 1024"));
        assert!(text.contains("paseo_relay_handshake_rejected_total 1"));
        assert!(text.contains("# TYPE paseo_relay_ready gauge"));
    }
}
