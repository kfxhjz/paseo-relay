//! Node-local resource accounting: WebSocket/TCP slots, per-IP limits, and the
//! weighted ingress byte budget.
//!
//! All accounting is explicit and bounded. Nothing here allocates per-byte
//! structures: a message holds at most one semaphore permit per fragment and a
//! small guard; payload bytes live in the connection's read buffer and in
//! bounded destination queues only.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::{Config, FIXED_MESSAGE_OVERHEAD, INGRESS_CHUNK_OVERHEAD};
use crate::metrics::Metrics;

#[derive(Debug, thiserror::Error)]
pub enum BudgetError {
    #[error("message exceeds ingress budget")]
    TooLarge,
    #[error("ingress budget exhausted")]
    Exhausted,
    #[error("ingress budget closed")]
    Closed,
}

pub struct IngressBudget {
    semaphore: Arc<Semaphore>,
    weight: u64,
    limit: u64,
    reserved: Arc<AtomicU64>,
}

impl IngressBudget {
    pub fn new(limit_bytes: u64, weight: u32) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(limit_bytes as usize)),
            weight: weight as u64,
            limit: limit_bytes,
            reserved: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn reserved_bytes(&self) -> u64 {
        self.reserved.load(Ordering::Relaxed)
    }

    /// Weighted charge for a payload of `payload_len` bytes, including the
    /// fixed per-message overhead. Empty frames are not free.
    pub fn weighted_bytes(&self, payload_len: usize) -> u64 {
        (payload_len as u64 + FIXED_MESSAGE_OVERHEAD) * self.weight
    }

    /// Conservative memory weight charged per payload byte.
    pub fn weight(&self) -> u64 {
        self.weight
    }

    /// Weighted charge for one payload chunk: payload bytes plus the fixed
    /// per-chunk bookkeeping charge (one budget guard and vector slack).
    pub fn chunk_weight(&self, payload_len: usize) -> u64 {
        (payload_len as u64)
            .saturating_mul(self.weight)
            .saturating_add(INGRESS_CHUNK_OVERHEAD)
    }

    /// Weighted charge for the fixed per-message bookkeeping overhead.
    pub fn overhead_weight(&self) -> u64 {
        FIXED_MESSAGE_OVERHEAD.saturating_mul(self.weight)
    }

    /// Reserve an already computed weighted amount before reading payload
    /// bytes. `timeout` bounds how long the source waits for budget.
    pub async fn reserve_weighted(
        &self,
        weighted: u64,
        timeout: Duration,
    ) -> Result<BudgetGuard, BudgetError> {
        if weighted > self.limit {
            return Err(BudgetError::TooLarge);
        }
        let weighted_u32 = u32::try_from(weighted).map_err(|_| BudgetError::TooLarge)?;
        let permit = tokio::time::timeout(
            timeout,
            self.semaphore.clone().acquire_many_owned(weighted_u32),
        )
        .await
        .map_err(|_| BudgetError::Exhausted)?
        .map_err(|_| BudgetError::Closed)?;
        self.reserved.fetch_add(weighted, Ordering::Relaxed);
        Ok(BudgetGuard {
            permit: Some(permit),
            weighted,
            reserved: Arc::clone(&self.reserved),
        })
    }

    /// Reserve the weighted bytes for a full payload message, including the
    /// fixed per-message overhead. Most callers should charge per chunk with
    /// [`IngressBudget::reserve_weighted`] instead.
    pub async fn reserve(
        &self,
        payload_len: usize,
        timeout: Duration,
    ) -> Result<BudgetGuard, BudgetError> {
        self.reserve_weighted(self.weighted_bytes(payload_len), timeout)
            .await
    }

    #[cfg(test)]
    pub async fn try_reserve(&self, payload_len: usize) -> Result<BudgetGuard, BudgetError> {
        let weighted = self.weighted_bytes(payload_len);
        if weighted > self.limit {
            return Err(BudgetError::TooLarge);
        }
        let weighted_u32 = u32::try_from(weighted).map_err(|_| BudgetError::TooLarge)?;
        let permit = self
            .semaphore
            .clone()
            .try_acquire_many_owned(weighted_u32)
            .map_err(|_| BudgetError::Exhausted)?;
        self.reserved.fetch_add(weighted, Ordering::Relaxed);
        Ok(BudgetGuard {
            permit: Some(permit),
            weighted,
            reserved: Arc::clone(&self.reserved),
        })
    }
}

/// RAII reservation. Dropping it returns weighted bytes to the budget, even on
/// cancellation or panic, which is the main reason this is safer than manual
/// release paths.
#[derive(Debug)]
pub struct BudgetGuard {
    permit: Option<OwnedSemaphorePermit>,
    weighted: u64,
    reserved: Arc<AtomicU64>,
}

impl BudgetGuard {
    pub fn weighted_bytes(&self) -> u64 {
        self.weighted
    }
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            self.reserved.fetch_sub(self.weighted, Ordering::Relaxed);
            drop(permit);
        }
    }
}

pub struct Capacity {
    max_connections: usize,
    ws_slots: Arc<Semaphore>,
    tcp_slots: Arc<Semaphore>,
    pub active_websockets: Arc<AtomicU64>,
    per_ip_max: usize,
    per_ip: Arc<Mutex<HashMap<IpAddr, usize>>>,
    pub ingress: IngressBudget,
    metrics: Arc<Metrics>,
}

impl Capacity {
    pub fn new(config: &Config, metrics: Arc<Metrics>) -> Self {
        Self {
            max_connections: config.max_connections,
            ws_slots: Arc::new(Semaphore::new(config.max_connections)),
            tcp_slots: Arc::new(Semaphore::new(config.max_tcp_connections)),
            active_websockets: Arc::new(AtomicU64::new(0)),
            per_ip_max: config.per_ip_max_connections,
            per_ip: Arc::new(Mutex::new(HashMap::new())),
            ingress: IngressBudget::new(config.ingress_budget_bytes, config.ingress_weight),
            metrics,
        }
    }

    pub fn try_acquire_ws(&self) -> Option<WsSlot> {
        match self.ws_slots.clone().try_acquire_owned() {
            Ok(permit) => {
                let active = self.active_websockets.fetch_add(1, Ordering::Relaxed) + 1;
                let _ = active;
                Some(WsSlot {
                    _permit: permit,
                    active: Arc::clone(&self.active_websockets),
                })
            }
            Err(_) => {
                Metrics::inc(&self.metrics.connection_rejections);
                None
            }
        }
    }

    pub fn try_acquire_tcp(&self) -> Option<TcpSlot> {
        self.tcp_slots
            .clone()
            .try_acquire_owned()
            .ok()
            .map(|permit| TcpSlot { _permit: permit })
    }

    pub fn try_acquire_ip(&self, ip: IpAddr) -> Result<IpGuard, PerIpRejection> {
        if self.per_ip_max == 0 {
            return Ok(IpGuard {
                ip,
                per_ip: Arc::clone(&self.per_ip),
                tracked: false,
            });
        }
        let mut map = self.per_ip.lock().expect("per-ip mutex poisoned");
        let count = map.entry(ip).or_insert(0);
        if *count >= self.per_ip_max {
            Metrics::inc(&self.metrics.per_ip_rejections);
            return Err(PerIpRejection);
        }
        *count += 1;
        Ok(IpGuard {
            ip,
            per_ip: Arc::clone(&self.per_ip),
            tracked: true,
        })
    }

    pub fn active_websockets(&self) -> u64 {
        self.active_websockets.load(Ordering::Relaxed)
    }

    pub fn websocket_capacity_open(&self) -> bool {
        self.active_websockets.load(Ordering::Relaxed) < self.max_connections as u64
    }

    pub fn ingress_reserved_bytes(&self) -> u64 {
        self.ingress.reserved_bytes()
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }
}

#[derive(Debug)]
pub struct WsSlot {
    _permit: OwnedSemaphorePermit,
    active: Arc<AtomicU64>,
}

impl Drop for WsSlot {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub struct TcpSlot {
    _permit: OwnedSemaphorePermit,
}

#[derive(Debug)]
pub struct IpGuard {
    ip: IpAddr,
    per_ip: Arc<Mutex<HashMap<IpAddr, usize>>>,
    tracked: bool,
}

impl Drop for IpGuard {
    fn drop(&mut self) {
        if !self.tracked {
            return;
        }
        let mut map = self.per_ip.lock().expect("per-ip mutex poisoned");
        if let Some(count) = map.get_mut(&self.ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(&self.ip);
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PerIpRejection;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_frames_are_not_free() {
        let budget = IngressBudget::new(10_000, 4);
        assert_eq!(budget.weighted_bytes(0), FIXED_MESSAGE_OVERHEAD * 4);
        assert_eq!(
            budget.weighted_bytes(100),
            (100 + FIXED_MESSAGE_OVERHEAD) * 4
        );
    }

    #[tokio::test]
    async fn budget_releases_on_guard_drop() {
        let budget = IngressBudget::new(4096, 1);
        assert_eq!(budget.reserved_bytes(), 0);
        {
            let _guard = budget.try_reserve(1000).await.unwrap();
            assert!(budget.reserved_bytes() > 0);
        }
        assert_eq!(budget.reserved_bytes(), 0);
    }

    #[tokio::test]
    async fn budget_exhausts_and_rejects() {
        let budget = IngressBudget::new(4096, 1);
        let first = budget.try_reserve(3000).await.unwrap();
        assert!(matches!(
            budget.try_reserve(3000).await,
            Err(BudgetError::Exhausted)
        ));
        drop(first);
        budget.try_reserve(3000).await.unwrap();
    }

    #[tokio::test]
    async fn wait_timeout_returns_exhausted() {
        let budget = IngressBudget::new(4096, 1);
        let _first = budget.try_reserve(3000).await.unwrap();
        let err = budget
            .reserve(3000, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(err, BudgetError::Exhausted));
    }

    #[test]
    fn per_ip_guard_tracks_counts() {
        let metrics = Arc::new(Metrics::new());
        let cfg = Config {
            per_ip_max_connections: 2,
            ..Config::default()
        };
        let capacity = Capacity::new(&cfg, metrics);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let first = capacity.try_acquire_ip(ip).unwrap();
        let second = capacity.try_acquire_ip(ip).unwrap();
        assert!(capacity.try_acquire_ip(ip).is_err());
        drop(first);
        capacity.try_acquire_ip(ip).unwrap();
        drop(second);
    }

    #[test]
    fn chunk_weight_includes_bookkeeping() {
        let budget = IngressBudget::new(1 << 20, 4);
        assert_eq!(
            budget.chunk_weight(1),
            4 + crate::config::INGRESS_CHUNK_OVERHEAD
        );
        assert_eq!(
            budget.chunk_weight(crate::config::INGRESS_CHUNK_BYTES),
            crate::config::INGRESS_CHUNK_BYTES as u64 * 4 + crate::config::INGRESS_CHUNK_OVERHEAD
        );
    }

    #[test]
    fn ws_slots_are_bounded() {
        let metrics = Arc::new(Metrics::new());
        let cfg = Config {
            max_connections: 2,
            max_tcp_connections: 2,
            ..Config::default()
        };
        let capacity = Capacity::new(&cfg, metrics);
        let first = capacity.try_acquire_ws().unwrap();
        let second = capacity.try_acquire_ws().unwrap();
        assert!(capacity.try_acquire_ws().is_none());
        assert_eq!(capacity.active_websockets(), 2);
        drop(first);
        assert_eq!(capacity.active_websockets(), 1);
        drop(second);
        assert_eq!(capacity.active_websockets(), 0);
    }
}
