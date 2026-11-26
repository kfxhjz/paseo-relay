//! Node-local resource accounting: WebSocket/TCP slots, per-IP limits, and the
//! weighted ingress byte budget.
//!
//! All accounting is explicit and bounded. Nothing here allocates per-byte
//! structures: a message holds at most one semaphore permit per chunk and a
//! small guard; payload bytes live in the connection's read buffer and in
//! bounded destination queues only.
//!
//! The ingress budget has two pools:
//!
//! * the **bulk** pool (`PASEO_RELAY_INGRESS_BUDGET_BYTES`) admits every data
//!   message;
//! * the **small reserve** (`PASEO_RELAY_INGRESS_SMALL_RESERVE_BYTES`) admits
//!   only messages of at most [`SMALL_MESSAGE_BYTES`], so handshakes and
//!   ordinary small frames keep flowing while large transfers saturate the
//!   bulk pool.
//!
//! Budget held by an *incomplete inbound message* is the one reservation whose
//! lifetime the sender controls. When another connection has to wait for
//! budget, [`IngressBudget::reserve`] reclaims such holds instead of only
//! shedding the waiter. In order:
//!
//! 1. holds that have stalled or are arriving below a minimum rate;
//! 2. once the waiter has spent half of its wait, a hold that has been
//!    arriving for longer than `PASEO_RELAY_INGRESS_RECLAIM_AFTER_MS` and
//!    longer than the waiter's own message (the oldest first), so a sender
//!    cannot keep budget by trickling just above the minimum rate;
//! 3. for a small-message waiter only, the largest hold if it is many times
//!    larger than the waiter.
//!
//! Nothing is reclaimed unless somebody is waiting, and a message that has
//! been received completely is never reclaimed: from then on the relay itself
//! bounds how long it is held (delivery and data-attach deadlines).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::config::{
    Config, FIXED_MESSAGE_OVERHEAD, INGRESS_CHUNK_BYTES, INGRESS_CHUNK_OVERHEAD,
    SMALL_MESSAGE_BYTES,
};
use crate::metrics::{Metrics, ShedKind};

/// Length of one progress bucket. A hold's receive rate is estimated over the
/// current bucket plus the previous one, i.e. its last one to two seconds.
const PROGRESS_BUCKET: Duration = Duration::from_secs(1);

/// A hold must have been receiving (budget waits excluded) for this long
/// before it can be judged slow. This gives a new message two full buckets.
const RECLAIM_MIN_AGE: Duration = Duration::from_secs(2);

/// Minimum spacing between two reclaim scans, node-wide. Every scan walks the
/// hold registry, so many simultaneous waiters must not multiply the work.
const RECLAIM_MIN_INTERVAL: Duration = Duration::from_millis(50);

/// Upper bound on holds reclaimed by one scan.
const RECLAIM_MAX_VICTIMS: usize = 64;

/// A small-message waiter may reclaim an in-progress hold that is at least
/// this many times larger than what the waiter itself would hold.
///
/// This and the age rule displace holds that are still making progress, so
/// they apply only after the waiter has spent half of its own wait: a healthy
/// transfer gets that long to finish or make room before it is sacrificed.
const SIZE_PRIORITY_RATIO: u64 = 8;

/// Longest and shortest pause between reclaim attempts of one waiter.
const RECLAIM_GRACE_MAX: Duration = Duration::from_millis(250);
const RECLAIM_GRACE_MIN: Duration = Duration::from_millis(5);

/// Recover a mutex guard if a previous panic poisoned it.
///
/// The guarded values are plain counters, maps and timer slots whose invariants
/// do not depend on a panicking critical section having completed; recovering
/// keeps one panic from turning into a permanent outage for every connection.
fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, thiserror::Error)]
pub enum BudgetError {
    #[error("message exceeds ingress budget")]
    TooLarge,
    #[error("ingress budget exhausted")]
    Exhausted,
    #[error("ingress budget closed")]
    Closed,
}

/// Which pools a reservation may draw from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The message is at most [`SMALL_MESSAGE_BYTES`] so far: small reserve
    /// first, bulk pool as a fallback.
    Small,
    /// Bulk pool only.
    Bulk,
}

impl Class {
    /// Class of a frame that brings its message to `message_bytes` payload
    /// bytes in total.
    pub fn for_message_bytes(message_bytes: usize) -> Self {
        if message_bytes <= SMALL_MESSAGE_BYTES {
            Class::Small
        } else {
            Class::Bulk
        }
    }
}

/// One budget request made while reading a data frame.
#[derive(Debug, Clone, Copy)]
pub struct Reservation {
    /// Weighted amount to reserve now.
    pub weighted: u64,
    pub class: Class,
    /// Weighted amount the rest of the current frame still needs. Only a hint
    /// that sizes how many stalled holds one reclaim scan frees.
    pub frame_need: u64,
}

impl Reservation {
    /// Reservation for the fixed per-message overhead, made before a data
    /// frame's payload is read.
    ///
    /// The reclaim hint is deliberately just this overhead: waiting for about
    /// 1 KiB must not size a reclaim scan for the whole declared frame.
    /// Otherwise a peer that sends only a multi-megabyte frame header and then
    /// stalls could evict many unrelated in-progress messages.
    pub fn overhead(weighted: u64, class: Class) -> Self {
        Self {
            weighted,
            class,
            frame_need: weighted,
        }
    }

    /// Reservation for one payload chunk of an in-progress frame.
    ///
    /// Only this phase may use the weight of the rest of the frame as the
    /// reclaim hint, because the bytes are actually being transferred.
    pub fn chunk(weighted: u64, class: Class, frame_need: u64) -> Self {
        Self {
            weighted,
            class,
            frame_need: frame_need.max(weighted),
        }
    }
}

/// Which ingress pool actually granted a reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolKind {
    Bulk,
    Small,
}

#[derive(Debug)]
struct Pool {
    kind: PoolKind,
    semaphore: Arc<Semaphore>,
    limit: u64,
    reserved: Arc<AtomicU64>,
}

impl Pool {
    fn new(kind: PoolKind, limit: u64) -> Self {
        Self {
            kind,
            semaphore: Arc::new(Semaphore::new(limit as usize)),
            limit,
            reserved: Arc::new(AtomicU64::new(0)),
        }
    }

    fn guard(&self, permit: OwnedSemaphorePermit, weighted: u64) -> BudgetGuard {
        self.reserved.fetch_add(weighted, Ordering::Relaxed);
        BudgetGuard {
            pool: self.kind,
            permit: Some(permit),
            weighted,
            reserved: Arc::clone(&self.reserved),
        }
    }

    fn try_acquire(&self, weighted: u64, permits: u32) -> Option<BudgetGuard> {
        if weighted > self.limit {
            return None;
        }
        let permit = Arc::clone(&self.semaphore)
            .try_acquire_many_owned(permits)
            .ok()?;
        Some(self.guard(permit, weighted))
    }
}

/// Progress bookkeeping for the one message a connection is currently
/// receiving. Times are measured on an *active clock* that stops while the
/// connection waits for budget, so a wait the relay imposed never makes a
/// sender look slow.
#[derive(Debug, Default)]
struct HoldState {
    /// Weighted bytes reserved from the bulk pool for the in-progress message.
    bulk_held: u64,
    /// Weighted bytes reserved from the small reserve for the in-progress message.
    small_held: u64,
    started: Option<Instant>,
    waiting_since: Option<Instant>,
    waited: Duration,
    /// Active-clock offset at which the current bucket began.
    bucket_start: Duration,
    /// Payload bytes received in the current and the previous bucket.
    current: u64,
    previous: u64,
}

impl HoldState {
    fn total_held(&self) -> u64 {
        self.bulk_held.saturating_add(self.small_held)
    }

    fn active(&self, now: Instant) -> Duration {
        let Some(started) = self.started else {
            return Duration::ZERO;
        };
        let mut waited = self.waited;
        if let Some(since) = self.waiting_since {
            waited += now.saturating_duration_since(since);
        }
        now.saturating_duration_since(started)
            .saturating_sub(waited)
    }

    fn roll(&mut self, active: Duration) {
        let elapsed = active.saturating_sub(self.bucket_start);
        if elapsed >= PROGRESS_BUCKET * 2 {
            let buckets = (elapsed.as_nanos() / PROGRESS_BUCKET.as_nanos()) as u32;
            self.previous = 0;
            self.current = 0;
            self.bucket_start += PROGRESS_BUCKET * buckets;
        } else if elapsed >= PROGRESS_BUCKET {
            self.previous = self.current;
            self.current = 0;
            self.bucket_start += PROGRESS_BUCKET;
        }
    }

    fn charged(&mut self, pool: PoolKind, weighted: u64, payload_bytes: u64, now: Instant) {
        if self.started.is_none() {
            // First charge of a new message. A reservation that had to wait
            // is charged after the wait ended, so the wait is not on this
            // message's active clock.
            *self = HoldState {
                started: Some(now),
                ..HoldState::default()
            };
        }
        match pool {
            PoolKind::Bulk => self.bulk_held = self.bulk_held.saturating_add(weighted),
            PoolKind::Small => self.small_held = self.small_held.saturating_add(weighted),
        }
        let active = self.active(now);
        self.roll(active);
        self.current = self.current.saturating_add(payload_bytes);
    }

    fn view(&mut self, now: Instant) -> HoldView {
        let active = self.active(now);
        self.roll(active);
        let window = PROGRESS_BUCKET + active.saturating_sub(self.bucket_start);
        let bytes = u128::from(self.previous) + u128::from(self.current);
        let rate = (bytes * 1_000_000_000 / window.as_nanos().max(1)) as u64;
        HoldView {
            bulk_held: self.bulk_held,
            small_held: self.small_held,
            active,
            rate,
            waiting: self.waiting_since.is_some(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct HoldView {
    bulk_held: u64,
    small_held: u64,
    active: Duration,
    /// Payload bytes per second over the trailing one to two seconds.
    rate: u64,
    waiting: bool,
}

impl HoldView {
    fn total_held(&self) -> u64 {
        self.bulk_held.saturating_add(self.small_held)
    }

    /// Weighted bytes held in pool(s) that can satisfy a waiter of `class`.
    /// A bulk waiter cannot use the small reserve, so holds purely in the
    /// small reserve are never relevant to it.
    fn for_class(&self, class: Class) -> u64 {
        match class {
            Class::Bulk => self.bulk_held,
            Class::Small => self.total_held(),
        }
    }
}

/// Per-connection record of the message currently being received.
#[derive(Debug)]
pub struct InboundHold {
    id: u64,
    evicted: CancellationToken,
    state: Mutex<HoldState>,
}

impl InboundHold {
    fn new(id: u64) -> Self {
        Self {
            id,
            evicted: CancellationToken::new(),
            state: Mutex::new(HoldState::default()),
        }
    }

    /// Token cancelled when this connection's in-progress message is
    /// reclaimed under budget pressure.
    pub fn evicted(&self) -> CancellationToken {
        self.evicted.clone()
    }

    fn view(&self, now: Instant) -> HoldView {
        lock_recover(&self.state).view(now)
    }

    fn wait_begin(&self, now: Instant) {
        let mut state = lock_recover(&self.state);
        if state.waiting_since.is_none() {
            state.waiting_since = Some(now);
        }
    }

    fn wait_end(&self, now: Instant) {
        let mut state = lock_recover(&self.state);
        if let Some(since) = state.waiting_since.take() {
            if state.started.is_some() {
                state.waited += now.saturating_duration_since(since);
            }
        }
    }
}

type HoldRegistry = Arc<Mutex<HashMap<u64, Arc<InboundHold>>>>;

/// Per-connection handle that inserts the connection into the active hold
/// registry only while an inbound message is actually in progress, so idle
/// WebSockets are never scanned during reclaim.
#[derive(Debug)]
pub struct HoldHandle {
    hold: Arc<InboundHold>,
    registry: HoldRegistry,
    registered: AtomicBool,
}

impl HoldHandle {
    /// Record a successful reservation of `weighted` bytes from `pool` that
    /// covered `payload_bytes` newly received payload bytes.
    pub fn charged(&self, pool: PoolKind, weighted: u64, payload_bytes: usize) {
        let now = Instant::now();
        {
            let mut state = lock_recover(&self.hold.state);
            state.charged(pool, weighted, payload_bytes as u64, now);
        }
        self.ensure_registered();
    }

    /// The in-progress message is complete (or abandoned): it is no longer an
    /// incomplete hold and cannot be reclaimed.
    pub fn message_done(&self) {
        {
            let mut state = lock_recover(&self.hold.state);
            *state = HoldState::default();
        }
        self.unregister();
    }

    fn ensure_registered(&self) {
        if !self.registered.swap(true, Ordering::AcqRel) {
            lock_recover(&self.registry).insert(self.hold.id, Arc::clone(&self.hold));
        }
    }

    fn unregister(&self) {
        if self.registered.swap(false, Ordering::AcqRel) {
            lock_recover(&self.registry).remove(&self.hold.id);
        }
    }
}

impl std::ops::Deref for HoldHandle {
    type Target = InboundHold;

    fn deref(&self) -> &InboundHold {
        &self.hold
    }
}

impl Drop for HoldHandle {
    fn drop(&mut self) {
        self.unregister();
    }
}

/// Separate reclaim-scan timestamps so impatient waiters cannot starve patient
/// waiters, and bulk waiters cannot starve small-message waiters.
#[derive(Debug, Default)]
struct ReclaimClock {
    slow: Option<Instant>,
    patient_bulk: Option<Instant>,
    patient_small: Option<Instant>,
}

struct WaitScope<'a>(&'a InboundHold);

impl Drop for WaitScope<'_> {
    fn drop(&mut self) {
        self.0.wait_end(Instant::now());
    }
}

pub struct IngressBudget {
    bulk: Pool,
    small: Pool,
    weight: u64,
    /// In-progress holds receiving at or below this many payload bytes per
    /// second are reclaimed first under pressure.
    reclaim_min_rate: u64,
    /// In-progress holds older than this yield to a newer message under
    /// pressure. Zero disables the rule.
    reclaim_after: Duration,
    holds: HoldRegistry,
    next_hold: AtomicU64,
    last_reclaim: Mutex<ReclaimClock>,
    metrics: Arc<Metrics>,
}

impl IngressBudget {
    pub fn new(
        limit_bytes: u64,
        small_reserve_bytes: u64,
        weight: u32,
        reclaim_min_rate: u64,
        reclaim_after: Duration,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            bulk: Pool::new(PoolKind::Bulk, limit_bytes),
            small: Pool::new(PoolKind::Small, small_reserve_bytes),
            weight: weight as u64,
            reclaim_min_rate,
            reclaim_after,
            holds: Arc::new(Mutex::new(HashMap::new())),
            next_hold: AtomicU64::new(1),
            last_reclaim: Mutex::new(ReclaimClock::default()),
            metrics,
        }
    }

    /// Bulk pool size (`PASEO_RELAY_INGRESS_BUDGET_BYTES`).
    pub fn limit(&self) -> u64 {
        self.bulk.limit
    }

    /// Weighted bytes currently reserved from the bulk pool.
    pub fn reserved_bytes(&self) -> u64 {
        self.bulk.reserved.load(Ordering::Relaxed)
    }

    /// Small reserve size (`PASEO_RELAY_INGRESS_SMALL_RESERVE_BYTES`).
    pub fn small_limit(&self) -> u64 {
        self.small.limit
    }

    /// Weighted bytes currently reserved from the small reserve.
    pub fn small_reserved_bytes(&self) -> u64 {
        self.small.reserved.load(Ordering::Relaxed)
    }

    /// Weighted charge for one payload chunk: payload bytes plus the fixed
    /// per-chunk bookkeeping charge (one budget guard and vector slack).
    pub fn chunk_weight(&self, payload_len: usize) -> u64 {
        (payload_len as u64)
            .saturating_mul(self.weight)
            .saturating_add(INGRESS_CHUNK_OVERHEAD)
    }

    /// Weighted charge for the fixed per-message bookkeeping overhead.
    /// Empty frames are not free.
    pub fn overhead_weight(&self) -> u64 {
        FIXED_MESSAGE_OVERHEAD.saturating_mul(self.weight)
    }

    /// Weighted charge for reading `payload_len` more bytes of one frame.
    pub fn frame_weight(&self, payload_len: usize) -> u64 {
        let chunks = payload_len.div_ceil(INGRESS_CHUNK_BYTES) as u64;
        (payload_len as u64)
            .saturating_mul(self.weight)
            .saturating_add(chunks.saturating_mul(INGRESS_CHUNK_OVERHEAD))
    }

    /// Create a hold handle for a connection. The connection enters the active
    /// hold registry only while it has an in-progress inbound message.
    pub fn register(&self) -> HoldHandle {
        let hold = Arc::new(InboundHold::new(
            self.next_hold.fetch_add(1, Ordering::Relaxed),
        ));
        HoldHandle {
            hold,
            registry: Arc::clone(&self.holds),
            registered: AtomicBool::new(false),
        }
    }

    /// Reserve weighted bytes before admitting payload that was just read.
    ///
    /// `timeout` bounds how long the source waits. While it waits, the budget
    /// periodically reclaims incomplete holds of other connections (see the
    /// module documentation); the waiter itself is closed by the caller only
    /// if nothing could be freed in time.
    pub async fn reserve(
        &self,
        hold: &InboundHold,
        request: Reservation,
        timeout: Duration,
    ) -> Result<BudgetGuard, BudgetError> {
        let weighted = request.weighted;
        if weighted > self.bulk.limit {
            return Err(BudgetError::TooLarge);
        }
        let permits = u32::try_from(weighted).map_err(|_| BudgetError::TooLarge)?;
        let small_eligible = request.class == Class::Small && weighted <= self.small.limit;

        if small_eligible {
            if let Some(guard) = self.small.try_acquire(weighted, permits) {
                return Ok(guard);
            }
        }
        if let Some(guard) = self.bulk.try_acquire(weighted, permits) {
            return Ok(guard);
        }

        // Boxed on purpose. The wait needs two semaphore acquisitions and two
        // timers; inlined, that state would be part of every connection's
        // read task and cost each idle socket several hundred bytes. It is
        // only needed while the budget is exhausted, so allocate it then.
        Box::pin(self.reserve_wait(hold, request, permits, small_eligible, timeout)).await
    }

    /// Slow path of [`Self::reserve`]: queue for budget and reclaim while
    /// waiting.
    async fn reserve_wait(
        &self,
        hold: &InboundHold,
        request: Reservation,
        permits: u32,
        small_eligible: bool,
        timeout: Duration,
    ) -> Result<BudgetGuard, BudgetError> {
        let weighted = request.weighted;
        let started = Instant::now();
        hold.wait_begin(started);
        let _waiting = WaitScope(hold);

        let bulk = Arc::clone(&self.bulk.semaphore).acquire_many_owned(permits);
        tokio::pin!(bulk);
        // A small waiter queues on both pools and takes whichever frees first.
        let small = async {
            if small_eligible {
                Arc::clone(&self.small.semaphore)
                    .acquire_many_owned(permits)
                    .await
            } else {
                std::future::pending().await
            }
        };
        tokio::pin!(small);

        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let grace = (timeout / 4).clamp(RECLAIM_GRACE_MIN, RECLAIM_GRACE_MAX);
        let mut reclaim = tokio::time::interval_at(tokio::time::Instant::now() + grace, grace);
        reclaim.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                permit = &mut small => {
                    let permit = permit.map_err(|_| BudgetError::Closed)?;
                    return Ok(self.small.guard(permit, weighted));
                }
                permit = &mut bulk => {
                    let permit = permit.map_err(|_| BudgetError::Closed)?;
                    return Ok(self.bulk.guard(permit, weighted));
                }
                _ = &mut deadline => return Err(BudgetError::Exhausted),
                _ = reclaim.tick() => {
                    let patient = started.elapsed() >= timeout / 2;
                    self.reclaim(hold, &request, patient);
                }
            }
        }
    }

    /// Free budget for `waiter` by cancelling incomplete holds of other
    /// connections. Victims are closed by their own connection task.
    ///
    /// Stalled and slow holds are always candidates. Once the waiter is
    /// `patient` (it has spent half of its wait), holds that are still making
    /// progress can be displaced too: one that has been arriving for too
    /// long, or, for a small-message waiter, one that is much larger.
    fn reclaim(&self, waiter: &InboundHold, request: &Reservation, patient: bool) {
        let now = Instant::now();
        {
            let mut clock = lock_recover(&self.last_reclaim);
            let slot = match (patient, request.class) {
                (false, _) => &mut clock.slow,
                (true, Class::Bulk) => &mut clock.patient_bulk,
                (true, Class::Small) => &mut clock.patient_small,
            };
            if slot.is_some_and(|at| now.saturating_duration_since(at) < RECLAIM_MIN_INTERVAL) {
                return;
            }
            *slot = Some(now);
        }

        let waiter_view = waiter.view(now);
        let waiter_held = waiter_view.total_held();
        let mut slow: Vec<(u64, Arc<InboundHold>)> = Vec::new();
        let mut old_candidates: Vec<(Duration, Arc<InboundHold>)> = Vec::new();
        let mut largest: Option<(u64, Arc<InboundHold>)> = None;
        {
            let holds = lock_recover(&self.holds);
            for hold in holds.values() {
                if hold.id == waiter.id || hold.evicted.is_cancelled() {
                    continue;
                }
                let view = hold.view(now);
                // Only inspect holds occupying a pool that can satisfy this
                // waiter: a bulk waiter cannot use the small reserve.
                let relevant = view.for_class(request.class);
                if relevant == 0 {
                    continue;
                }
                // A hold that is itself waiting for budget is stalled by the
                // relay, not by its sender, so it is never judged slow.
                if !view.waiting
                    && view.active >= RECLAIM_MIN_AGE
                    && view.rate <= self.reclaim_min_rate
                {
                    slow.push((relevant, Arc::clone(hold)));
                }
                if largest.as_ref().is_none_or(|(held, _)| relevant > *held) {
                    largest = Some((relevant, Arc::clone(hold)));
                }
                // Age is measured on the active clock, so time a message
                // spent waiting for budget is not held against it. That is
                // also why a hold that is waiting right now is not exempt
                // here, unlike above: its age has stopped growing, and a
                // sender that fills the pool and then blocks in its own
                // reservation would otherwise be protected from the very
                // waiters it is starving.
                if !self.reclaim_after.is_zero()
                    && view.active >= self.reclaim_after
                    && view.active > waiter_view.active
                {
                    old_candidates.push((view.active, Arc::clone(hold)));
                }
            }
        }

        let need = request.frame_need.max(request.weighted);
        let mut freed = 0u64;
        let mut victims = 0usize;
        if !slow.is_empty() {
            slow.sort_unstable_by_key(|(held, _)| std::cmp::Reverse(*held));
            for (_, hold) in slow.into_iter().take(RECLAIM_MAX_VICTIMS) {
                if let Some(bytes) = self.evict(&hold, request.class) {
                    freed = freed.saturating_add(bytes);
                    victims += 1;
                    if freed >= need || victims >= RECLAIM_MAX_VICTIMS {
                        return;
                    }
                }
            }
        }

        if !patient {
            return;
        }

        // A message that has been arriving for a long time yields to a newer
        // one. Without this a sender could keep budget indefinitely by
        // trickling just above the slow threshold; with it, holding budget
        // costs re-sending the held bytes every `reclaim_after`. Continue until
        // `freed >= need` (bounded by `RECLAIM_MAX_VICTIMS`) so tiny old holds
        // cannot shield a large old hold across the waiter's timeout window.
        if !old_candidates.is_empty() {
            old_candidates.sort_unstable_by_key(|(age, _)| std::cmp::Reverse(*age));
            for (_, hold) in old_candidates {
                if victims >= RECLAIM_MAX_VICTIMS {
                    return;
                }
                if hold.evicted.is_cancelled() {
                    continue;
                }
                if let Some(bytes) = self.evict(&hold, request.class) {
                    freed = freed.saturating_add(bytes);
                    victims += 1;
                    if freed >= need {
                        return;
                    }
                }
            }
        }

        // Small messages outrank a much larger in-progress transfer, so they
        // do not have to wait for it to grow old.
        if request.class == Class::Small {
            if let Some((held, hold)) = largest {
                if !hold.evicted.is_cancelled() {
                    let own = waiter_held.saturating_add(request.weighted);
                    if held >= own.saturating_mul(SIZE_PRIORITY_RATIO) {
                        let _ = self.evict(&hold, request.class);
                    }
                }
            }
        }
    }

    /// Cancel `hold`'s in-progress message. Returns the weighted bytes this
    /// frees in pools usable by `class`, or `None` if the message completed or
    /// was already evicted.
    fn evict(&self, hold: &InboundHold, class: Class) -> Option<u64> {
        let freed = {
            // `message_done` takes the same lock, so a message that has just
            // completed is never mistaken for an incomplete one.
            let state = lock_recover(&hold.state);
            let relevant = match class {
                Class::Bulk => state.bulk_held,
                Class::Small => state.total_held(),
            };
            if relevant == 0 || hold.evicted.is_cancelled() {
                return None;
            }
            hold.evicted.cancel();
            relevant
        };
        Metrics::inc(&self.metrics.ingress_reclaims);
        self.metrics.shed(ShedKind::IngressReclaimed);
        Some(freed)
    }

    #[cfg(test)]
    fn try_reserve(&self, weighted: u64, class: Class) -> Result<BudgetGuard, BudgetError> {
        let permits = u32::try_from(weighted).map_err(|_| BudgetError::TooLarge)?;
        if class == Class::Small {
            if let Some(guard) = self.small.try_acquire(weighted, permits) {
                return Ok(guard);
            }
        }
        self.bulk
            .try_acquire(weighted, permits)
            .ok_or(BudgetError::Exhausted)
    }
}

/// RAII reservation. Dropping it returns weighted bytes to its pool, even on
/// cancellation or panic, which is the main reason this is safer than manual
/// release paths.
#[derive(Debug)]
pub struct BudgetGuard {
    pool: PoolKind,
    permit: Option<OwnedSemaphorePermit>,
    weighted: u64,
    reserved: Arc<AtomicU64>,
}

impl BudgetGuard {
    pub fn weighted_bytes(&self) -> u64 {
        self.weighted
    }

    pub fn pool(&self) -> PoolKind {
        self.pool
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

/// Per-connection in-flight ingress budget.
///
/// Guards share this counter; one connection may reserve at most
/// `min(ingress budget, 4 × one-max-message charge)` weighted bytes, so that
/// on a node with a large budget a single peer cannot take all of it through
/// queued messages.
#[derive(Debug)]
pub struct ConnIngress {
    used: AtomicU64,
    limit: u64,
}

impl ConnIngress {
    pub fn new(limit: u64) -> Self {
        Self {
            used: AtomicU64::new(0),
            limit,
        }
    }

    pub fn try_scope(self: &Arc<Self>, guard: BudgetGuard) -> Result<ScopedGuard, BudgetError> {
        let weighted = guard.weighted_bytes();
        let previous = self.used.fetch_add(weighted, Ordering::Relaxed);
        if previous.saturating_add(weighted) > self.limit {
            self.used.fetch_sub(weighted, Ordering::Relaxed);
            return Err(BudgetError::Exhausted);
        }
        Ok(ScopedGuard {
            guard: Some(guard),
            scope: Arc::clone(self),
            weighted,
        })
    }
}

/// Ingress guard that also releases the connection-scoped counter.
#[derive(Debug)]
pub struct ScopedGuard {
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

/// Shared RAII handle holding the ingress budget for one complete message until
/// every destination writer that accepted it has finished writing or dropped
/// the frame.
#[derive(Debug)]
pub struct BudgetHold {
    _guards: Vec<ScopedGuard>,
}

impl BudgetHold {
    pub fn new(guards: Vec<ScopedGuard>) -> Arc<Self> {
        Arc::new(Self { _guards: guards })
    }
}

/// Key used for per-IP accounting.
///
/// IPv6 clients are grouped by /64, the smallest prefix a network normally
/// delegates to one subscriber: counting full addresses would let a single
/// host rotate through its prefix and never reach a limit. IPv4-mapped IPv6
/// addresses are counted as their IPv4 address.
pub fn ip_limit_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
    }
}

/// Open sockets per [`ip_limit_key`].
type IpCounts = Arc<Mutex<HashMap<IpAddr, usize>>>;

#[derive(Debug)]
struct IpLimiter {
    /// `0` disables the limit.
    max: usize,
    counts: IpCounts,
}

impl IpLimiter {
    fn new(max: usize) -> Self {
        Self {
            max,
            counts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn try_acquire(&self, ip: IpAddr) -> Option<IpGuard> {
        if self.max == 0 {
            return Some(IpGuard { tracked: None });
        }
        let key = ip_limit_key(ip);
        let mut counts = lock_recover(&self.counts);
        let count = counts.entry(key).or_insert(0);
        if *count >= self.max {
            return None;
        }
        *count += 1;
        Some(IpGuard {
            tracked: Some((key, Arc::clone(&self.counts))),
        })
    }
}

#[derive(Debug)]
pub struct IpGuard {
    tracked: Option<(IpAddr, IpCounts)>,
}

impl Drop for IpGuard {
    fn drop(&mut self) {
        let Some((key, counts)) = self.tracked.take() else {
            return;
        };
        let mut counts = lock_recover(&counts);
        if let Some(count) = counts.get_mut(&key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&key);
            }
        }
    }
}

/// Per-IP cap on open TCP sockets derived from the WebSocket cap.
///
/// A client needs a little room above its WebSocket allowance for sockets
/// that are still in the HTTP phase, so the TCP cap is the WebSocket cap plus
/// a quarter (at least 8). `0` keeps the limit disabled.
pub fn per_ip_tcp_limit(per_ip_max_connections: usize) -> usize {
    if per_ip_max_connections == 0 {
        0
    } else {
        per_ip_max_connections.saturating_add((per_ip_max_connections / 4).max(8))
    }
}

pub struct Capacity {
    max_connections: usize,
    ws_slots: Arc<Semaphore>,
    tcp_slots: Arc<Semaphore>,
    pub active_websockets: Arc<AtomicU64>,
    per_ip_ws: IpLimiter,
    per_ip_tcp: IpLimiter,
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
            per_ip_ws: IpLimiter::new(config.per_ip_max_connections),
            per_ip_tcp: IpLimiter::new(per_ip_tcp_limit(config.per_ip_max_connections)),
            ingress: IngressBudget::new(
                config.ingress_budget_bytes,
                config.ingress_small_reserve_bytes,
                config.ingress_weight,
                config.ingress_reclaim_min_rate_bytes,
                config.ingress_reclaim_after,
                Arc::clone(&metrics),
            ),
            metrics,
        }
    }

    pub fn try_acquire_ws(&self) -> Option<WsSlot> {
        match self.ws_slots.clone().try_acquire_owned() {
            Ok(permit) => {
                self.active_websockets.fetch_add(1, Ordering::Relaxed);
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

    /// Admit one accepted TCP socket from `ip`. The slot is held for the
    /// whole socket lifetime, upgraded WebSockets included.
    pub fn try_acquire_tcp(&self, ip: IpAddr) -> Result<TcpSlot, TcpRejection> {
        let permit = self
            .tcp_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| TcpRejection::Capacity)?;
        let ip = self.per_ip_tcp.try_acquire(ip).ok_or(TcpRejection::PerIp)?;
        Ok(TcpSlot {
            _permit: permit,
            _ip: ip,
        })
    }

    /// Admit one WebSocket from `ip` against the per-IP WebSocket cap.
    pub fn try_acquire_ip(&self, ip: IpAddr) -> Result<IpGuard, PerIpRejection> {
        self.per_ip_ws.try_acquire(ip).ok_or_else(|| {
            Metrics::inc(&self.metrics.per_ip_rejections);
            PerIpRejection
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
    _ip: IpGuard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpRejection {
    /// `PASEO_RELAY_MAX_TCP_CONNECTIONS` reached.
    Capacity,
    /// This peer already holds its per-IP share of TCP sockets.
    PerIp,
}

#[derive(Debug, Clone, Copy)]
pub struct PerIpRejection;

#[cfg(test)]
mod tests {
    use super::*;

    const RECLAIM_AFTER: Duration = Duration::from_secs(60);

    fn budget(limit: u64, small: u64) -> IngressBudget {
        budget_with_age(limit, small, RECLAIM_AFTER)
    }

    fn budget_with_age(limit: u64, small: u64, reclaim_after: Duration) -> IngressBudget {
        IngressBudget::new(
            limit,
            small,
            4,
            64 * 1024,
            reclaim_after,
            Arc::new(Metrics::new()),
        )
    }

    /// Make `hold` look like a message that started `age` ago and has been
    /// receiving `rate` payload bytes per second ever since.
    ///
    /// Nothing keeps feeding the hold afterwards: within about a second it
    /// reads as stalled, so tests that need it healthy must look sooner.
    fn receiving_for(hold: &HoldHandle, weighted: u64, age: Duration, rate: u64) {
        receiving_for_pool(hold, PoolKind::Bulk, weighted, age, rate);
    }

    fn receiving_for_pool(
        hold: &HoldHandle,
        pool: PoolKind,
        weighted: u64,
        age: Duration,
        rate: u64,
    ) {
        {
            let mut state = hold.state.lock().unwrap();
            *state = HoldState::default();
            let start = Instant::now() - age;
            let steps = (age.as_millis() / 100) as u32;
            state.charged(pool, weighted, rate / 10, start);
            for step in 1..steps {
                state.charged(
                    pool,
                    0,
                    rate / 10,
                    start + Duration::from_millis(100) * step,
                );
            }
        }
        hold.ensure_registered();
    }

    fn bulk(weighted: u64) -> Reservation {
        Reservation {
            weighted,
            class: Class::Bulk,
            frame_need: weighted,
        }
    }

    fn small(weighted: u64) -> Reservation {
        Reservation {
            weighted,
            class: Class::Small,
            frame_need: weighted,
        }
    }

    #[test]
    fn empty_frames_are_not_free() {
        let budget = budget(10_000, 0);
        assert_eq!(budget.overhead_weight(), FIXED_MESSAGE_OVERHEAD * 4);
    }

    #[test]
    fn chunk_and_frame_weights_include_bookkeeping() {
        let budget = budget(1 << 20, 0);
        assert_eq!(budget.chunk_weight(1), 4 + INGRESS_CHUNK_OVERHEAD);
        assert_eq!(
            budget.chunk_weight(INGRESS_CHUNK_BYTES),
            INGRESS_CHUNK_BYTES as u64 * 4 + INGRESS_CHUNK_OVERHEAD
        );
        assert_eq!(budget.frame_weight(0), 0);
        assert_eq!(
            budget.frame_weight(INGRESS_CHUNK_BYTES + 1),
            (INGRESS_CHUNK_BYTES as u64 + 1) * 4 + 2 * INGRESS_CHUNK_OVERHEAD
        );
    }

    #[test]
    fn overhead_reservation_does_not_size_a_whole_frame_reclaim() {
        // A ~1 KiB overhead wait may free only what it needs: one stalled
        // in-progress message holding 4 KiB is enough. Passing the declared
        // frame weight here (as the old code did) would evict several
        // unrelated in-progress messages for a header that never sent payload.
        let budget = budget(100_000, 0);
        let _guard = budget.try_reserve(100_000, Class::Bulk).unwrap();
        let stalled: Vec<HoldHandle> = (0..4)
            .map(|_| {
                let hold = budget.register();
                backdate(&hold, 4096, 1024, Duration::from_secs(5));
                hold
            })
            .collect();
        let waiter = budget.register();
        let request = Reservation::overhead(budget.overhead_weight(), Class::Bulk);
        assert_eq!(request.frame_need, request.weighted);
        budget.reclaim(&waiter, &request, false);
        let evicted = stalled
            .iter()
            .filter(|hold| hold.evicted().is_cancelled())
            .count();
        assert_eq!(
            evicted, 1,
            "a 1 KiB overhead wait must not evict a whole frame's worth of victims"
        );

        // Only the chunk phase sizes the scan for the rest of the frame.
        let chunk = Reservation::chunk(4096, Class::Bulk, 32 * 1024 * 1024);
        assert_eq!(chunk.frame_need, 32 * 1024 * 1024);
    }

    #[test]
    fn budget_releases_on_guard_drop() {
        let budget = budget(4096, 0);
        assert_eq!(budget.reserved_bytes(), 0);
        {
            let _guard = budget.try_reserve(1000, Class::Bulk).unwrap();
            assert_eq!(budget.reserved_bytes(), 1000);
        }
        assert_eq!(budget.reserved_bytes(), 0);
    }

    #[test]
    fn budget_exhausts_and_rejects() {
        let budget = budget(4096, 0);
        let first = budget.try_reserve(3000, Class::Bulk).unwrap();
        assert!(matches!(
            budget.try_reserve(3000, Class::Bulk),
            Err(BudgetError::Exhausted)
        ));
        drop(first);
        budget.try_reserve(3000, Class::Bulk).unwrap();
    }

    #[test]
    fn small_reserve_is_closed_to_bulk_and_falls_back_to_bulk() {
        let budget = budget(4096, 2048);
        // Bulk traffic can fill the bulk pool but never touches the reserve.
        let _bulk = budget.try_reserve(4096, Class::Bulk).unwrap();
        assert!(budget.try_reserve(1, Class::Bulk).is_err());
        assert_eq!(budget.small_reserved_bytes(), 0);

        // Small traffic still gets through on the reserve.
        let first = budget.try_reserve(2048, Class::Small).unwrap();
        assert_eq!(budget.small_reserved_bytes(), 2048);
        assert!(budget.try_reserve(1, Class::Small).is_err());
        drop(first);
        drop(_bulk);

        // With the reserve taken, small traffic falls back to the bulk pool.
        let _reserve = budget.try_reserve(2048, Class::Small).unwrap();
        let _fallback = budget.try_reserve(1000, Class::Small).unwrap();
        assert_eq!(budget.reserved_bytes(), 1000);
    }

    #[tokio::test]
    async fn wait_timeout_returns_exhausted() {
        let budget = budget(4096, 0);
        let hold = budget.register();
        let _first = budget.try_reserve(3000, Class::Bulk).unwrap();
        let err = budget
            .reserve(&hold, bulk(3000), Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(err, BudgetError::Exhausted));
    }

    #[tokio::test]
    async fn oversized_reservation_is_too_large() {
        let budget = budget(4096, 0);
        let hold = budget.register();
        let err = budget
            .reserve(&hold, bulk(4097), Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(err, BudgetError::TooLarge));
    }

    #[tokio::test]
    async fn small_waiter_takes_whichever_pool_frees_first() {
        let budget = Arc::new(budget(4096, 1024));
        let hold = budget.register();
        let bulk_guard = budget.try_reserve(4096, Class::Bulk).unwrap();
        let small_guard = budget.try_reserve(1024, Class::Small).unwrap();

        let waiter = {
            let budget = Arc::clone(&budget);
            tokio::spawn(async move {
                budget
                    .reserve(&hold, small(512), Duration::from_secs(5))
                    .await
                    .map(|guard| guard.weighted_bytes())
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(small_guard);
        assert_eq!(waiter.await.unwrap().unwrap(), 512);
        assert_eq!(budget.reserved_bytes(), 4096);
        drop(bulk_guard);
    }

    #[test]
    fn hold_rate_ignores_time_spent_waiting_for_budget() {
        let start = Instant::now();
        let mut state = HoldState::default();
        state.charged(PoolKind::Bulk, 4096, 1024 * 1024, start);
        // Three seconds pass, all of them waiting for budget.
        state.waiting_since = Some(start);
        let view = state.view(start + Duration::from_secs(3));
        assert!(view.waiting);
        assert_eq!(view.active, Duration::ZERO);
        assert!(view.rate >= 512 * 1024, "rate {}", view.rate);
    }

    #[test]
    fn hold_rate_decays_once_the_sender_stalls() {
        let start = Instant::now();
        let mut state = HoldState::default();
        state.charged(PoolKind::Bulk, 4096, 8 * 1024 * 1024, start);
        let fresh = state.view(start + Duration::from_millis(500));
        assert!(fresh.rate >= 4 * 1024 * 1024);
        let stalled = state.view(start + Duration::from_secs(3));
        assert_eq!(stalled.rate, 0);
        assert_eq!(stalled.bulk_held, 4096);
        assert!(stalled.active >= RECLAIM_MIN_AGE);
    }

    #[test]
    fn hold_rate_tracks_a_steady_sender() {
        let start = Instant::now();
        let mut state = HoldState::default();
        // 100 KiB every 100 ms for five seconds = 1 MiB/s.
        for step in 0..50u32 {
            state.charged(
                PoolKind::Bulk,
                1,
                100 * 1024,
                start + Duration::from_millis(100) * step,
            );
        }
        let view = state.view(start + Duration::from_secs(5));
        assert!(
            (900 * 1024..=1100 * 1024).contains(&view.rate),
            "rate {}",
            view.rate
        );
    }

    /// Make `hold` look like a message that received `payload` bytes and has
    /// been silent for `silent`.
    fn backdate(hold: &HoldHandle, weighted: u64, payload: u64, silent: Duration) {
        backdate_pool(hold, PoolKind::Bulk, weighted, payload, silent);
    }

    fn backdate_pool(
        hold: &HoldHandle,
        pool: PoolKind,
        weighted: u64,
        payload: u64,
        silent: Duration,
    ) {
        {
            let mut state = hold.state.lock().unwrap();
            *state = HoldState::default();
            state.charged(pool, weighted, payload, Instant::now() - silent);
        }
        hold.ensure_registered();
    }

    #[tokio::test]
    async fn stalled_hold_is_reclaimed_for_a_waiter() {
        let budget = budget(4096, 0);
        let stalled = budget.register();
        let _stalled_guard = budget.try_reserve(4096, Class::Bulk).unwrap();
        backdate(&stalled, 4096, 1024, Duration::from_secs(5));

        let waiter = budget.register();
        let result = budget
            .reserve(&waiter, bulk(1024), Duration::from_millis(200))
            .await;
        // The guard is still alive in this test, so the wait itself fails;
        // what matters is that the stalled connection was told to go.
        assert!(result.is_err());
        assert!(stalled.evicted().is_cancelled());
        assert!(!waiter.evicted().is_cancelled());
    }

    #[tokio::test]
    async fn healthy_hold_survives_a_bulk_waiter() {
        let budget = budget(4096, 0);
        let healthy = budget.register();
        let _guard = budget.try_reserve(4096, Class::Bulk).unwrap();
        // Old enough to be judged, but still receiving well above the floor.
        {
            let mut state = healthy.state.lock().unwrap();
            let start = Instant::now() - Duration::from_secs(4);
            for step in 0..40u32 {
                state.charged(
                    PoolKind::Bulk,
                    100,
                    1024 * 1024,
                    start + Duration::from_millis(100) * step,
                );
            }
        }
        healthy.ensure_registered();
        let waiter = budget.register();
        let result = budget
            .reserve(&waiter, bulk(1024), Duration::from_millis(120))
            .await;
        assert!(matches!(result, Err(BudgetError::Exhausted)));
        assert!(!healthy.evicted().is_cancelled());
    }

    #[tokio::test]
    async fn small_waiter_reclaims_a_much_larger_active_hold() {
        let budget = budget(1 << 20, 0);
        let large = budget.register();
        let _guard = budget.try_reserve(1 << 20, Class::Bulk).unwrap();
        // Brand new and fast: not slow by any measure, only large.
        large.charged(PoolKind::Bulk, 1 << 20, 256 * 1024);

        let waiter = budget.register();
        let started = Instant::now();
        let waiting = budget.reserve(&waiter, small(1024), Duration::from_millis(600));
        tokio::pin!(waiting);
        // Not during the first half of the wait: the transfer may yet finish.
        let _ = tokio::time::timeout(Duration::from_millis(200), &mut waiting).await;
        assert!(!large.evicted().is_cancelled());
        let _ = waiting.await;
        assert!(large.evicted().is_cancelled());
        assert!(started.elapsed() >= Duration::from_millis(300));
    }

    #[tokio::test]
    async fn bulk_waiter_never_reclaims_by_size_alone() {
        let budget = budget(1 << 20, 0);
        let large = budget.register();
        let _guard = budget.try_reserve(1 << 20, Class::Bulk).unwrap();
        large.charged(PoolKind::Bulk, 1 << 20, 256 * 1024);

        let waiter = budget.register();
        let _ = budget
            .reserve(&waiter, bulk(1024), Duration::from_millis(120))
            .await;
        assert!(!large.evicted().is_cancelled());
    }

    #[tokio::test]
    async fn a_hold_waiting_for_budget_is_not_judged_slow() {
        let budget = budget(4096, 0);
        let blocked = budget.register();
        let _guard = budget.try_reserve(4096, Class::Bulk).unwrap();
        backdate(&blocked, 2048, 1024, Duration::from_secs(5));
        blocked.wait_begin(Instant::now() - Duration::from_secs(5));

        let waiter = budget.register();
        let _ = budget
            .reserve(&waiter, bulk(1024), Duration::from_millis(120))
            .await;
        assert!(!blocked.evicted().is_cancelled());
    }

    #[tokio::test]
    async fn an_old_message_yields_to_a_newer_one_even_if_it_is_not_slow() {
        let budget = budget(4096, 0);
        let old = budget.register();
        let _guard = budget.try_reserve(4096, Class::Bulk).unwrap();
        // Trickling at 1 MiB/s for 90 s: far above the slow floor, but it
        // has held its budget for longer than the 60 s allowance.
        receiving_for(&old, 4096, Duration::from_secs(90), 1024 * 1024);

        let waiter = budget.register();
        let waiting = budget.reserve(&waiter, bulk(1024), Duration::from_millis(600));
        tokio::pin!(waiting);
        // Not during the first half of the wait.
        let _ = tokio::time::timeout(Duration::from_millis(200), &mut waiting).await;
        assert!(!old.evicted().is_cancelled());
        let _ = waiting.await;
        assert!(old.evicted().is_cancelled());
    }

    #[tokio::test]
    async fn a_message_within_its_allowance_is_not_displaced_by_age() {
        let budget = budget(4096, 0);
        let young = budget.register();
        let _guard = budget.try_reserve(4096, Class::Bulk).unwrap();
        receiving_for(&young, 4096, Duration::from_secs(30), 1024 * 1024);

        let waiter = budget.register();
        let _ = budget
            .reserve(&waiter, bulk(1024), Duration::from_millis(300))
            .await;
        assert!(!young.evicted().is_cancelled());
    }

    #[tokio::test]
    async fn an_older_waiter_does_not_displace_a_younger_message() {
        let budget = budget(8192, 0);
        let holder = budget.register();
        let _guard = budget.try_reserve(8192, Class::Bulk).unwrap();
        receiving_for(&holder, 4096, Duration::from_secs(90), 1024 * 1024);

        // The waiter's own message has been arriving for even longer, so it
        // has no claim on the holder's budget.
        let waiter = budget.register();
        receiving_for(&waiter, 4096, Duration::from_secs(120), 1024 * 1024);
        let _ = budget
            .reserve(&waiter, bulk(1024), Duration::from_millis(300))
            .await;
        assert!(!holder.evicted().is_cancelled());
    }

    #[tokio::test]
    async fn an_old_message_yields_even_while_it_waits_for_budget_itself() {
        let budget = budget(4096, 0);
        let old = budget.register();
        let _guard = budget.try_reserve(4096, Class::Bulk).unwrap();
        // 90 s of receiving, then blocked in its own reservation: the pool is
        // full because of what it already holds.
        receiving_for(&old, 4096, Duration::from_secs(92), 1024 * 1024);
        old.wait_begin(Instant::now() - Duration::from_secs(2));

        let waiter = budget.register();
        let _ = budget
            .reserve(&waiter, bulk(1024), Duration::from_millis(300))
            .await;
        assert!(old.evicted().is_cancelled());
    }

    #[tokio::test]
    async fn the_age_rule_can_be_disabled() {
        let budget = budget_with_age(4096, 0, Duration::ZERO);
        let old = budget.register();
        let _guard = budget.try_reserve(4096, Class::Bulk).unwrap();
        receiving_for(&old, 4096, Duration::from_secs(300), 1024 * 1024);

        let waiter = budget.register();
        let _ = budget
            .reserve(&waiter, bulk(1024), Duration::from_millis(300))
            .await;
        assert!(!old.evicted().is_cancelled());
    }

    #[tokio::test]
    async fn the_oldest_message_is_displaced_first() {
        let budget = budget(8192, 0);
        let _guard = budget.try_reserve(8192, Class::Bulk).unwrap();
        let older = budget.register();
        let newer = budget.register();
        receiving_for(&older, 4096, Duration::from_secs(150), 1024 * 1024);
        receiving_for(&newer, 4096, Duration::from_secs(90), 1024 * 1024);

        let waiter = budget.register();
        // Keep the wait short: `receiving_for` stops feeding the holds when it
        // returns, so after a second they would count as stalled instead.
        let waiting = budget.reserve(&waiter, bulk(1024), Duration::from_millis(400));
        tokio::pin!(waiting);
        // Stop at the first eviction: a later scan would rightly take the
        // next-oldest message too.
        tokio::select! {
            biased;
            _ = older.evicted.cancelled() => {}
            _ = newer.evicted.cancelled() => {}
            _ = &mut waiting => panic!("no message was displaced"),
        }
        assert!(older.evicted().is_cancelled());
        assert!(!newer.evicted().is_cancelled());
    }

    #[tokio::test]
    async fn a_message_that_completed_is_not_reclaimed() {
        let budget = budget(4096, 0);
        let finished = budget.register();
        // Its budget is still reserved (the message is queued for delivery),
        // but it is no longer an incomplete inbound message.
        let _guard = budget.try_reserve(4096, Class::Bulk).unwrap();
        backdate(&finished, 4096, 1024, Duration::from_secs(5));
        finished.message_done();

        let waiter = budget.register();
        let _ = budget
            .reserve(&waiter, bulk(1024), Duration::from_millis(120))
            .await;
        assert!(!finished.evicted().is_cancelled());
        assert_eq!(budget.evict(&finished, Class::Bulk), None);
    }

    #[test]
    fn idle_handles_stay_out_of_the_registry_and_active_ones_unregister() {
        let budget = budget(4096, 0);
        let handle = budget.register();
        assert!(budget.holds.lock().unwrap().is_empty());
        handle.charged(PoolKind::Bulk, 1024, 256);
        assert_eq!(budget.holds.lock().unwrap().len(), 1);
        handle.message_done();
        assert!(budget.holds.lock().unwrap().is_empty());
        handle.charged(PoolKind::Bulk, 1024, 256);
        assert_eq!(budget.holds.lock().unwrap().len(), 1);
        drop(handle);
        assert!(budget.holds.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn bulk_waiter_ignores_stalled_small_reserve_holds_and_evicts_old_bulk() {
        let budget = budget(4096, 2048);
        let _bulk_guard = budget.try_reserve(4096, Class::Bulk).unwrap();
        let _small_guard = budget.try_reserve(2048, Class::Small).unwrap();

        let small_stalled = budget.register();
        backdate_pool(
            &small_stalled,
            PoolKind::Small,
            2048,
            512,
            Duration::from_secs(5),
        );

        let old_bulk = budget.register();
        receiving_for(&old_bulk, 4096, Duration::from_secs(90), 1024 * 1024);

        let waiter = budget.register();
        let _ = budget
            .reserve(&waiter, bulk(1024), Duration::from_millis(300))
            .await;
        assert!(
            !small_stalled.evicted().is_cancelled(),
            "bulk waiter must not evict a small-reserve-only hold"
        );
        assert!(
            old_bulk.evicted().is_cancelled(),
            "bulk waiter must still evict the old bulk hold"
        );
    }

    #[tokio::test]
    async fn tiny_stalled_decoys_do_not_shield_an_old_bulk_hold() {
        let budget = budget(100_000, 0);
        let _guard = budget.try_reserve(100_000, Class::Bulk).unwrap();

        let decoy = budget.register();
        backdate(&decoy, 512, 128, Duration::from_secs(5));

        let old = budget.register();
        receiving_for(&old, 99_488, Duration::from_secs(90), 1024 * 1024);

        let waiter = budget.register();
        let _ = budget
            .reserve(&waiter, bulk(10_000), Duration::from_millis(300))
            .await;
        assert!(decoy.evicted().is_cancelled());
        assert!(
            old.evicted().is_cancelled(),
            "evicting a tiny decoy below `need` must not stop the age rule"
        );
    }

    #[tokio::test]
    async fn tiny_old_holds_do_not_shield_a_large_old_hold() {
        let budget = budget(100_000, 0);
        let _guard = budget.try_reserve(100_000, Class::Bulk).unwrap();

        // Twelve slightly older tiny holds (not slow) sit ahead of a large old
        // hold in age order; evicting them one-by-one per tick would starve the
        // waiter across its timeout window.
        let mut tiny_holds = Vec::new();
        for _ in 0..12 {
            let h = budget.register();
            receiving_for(&h, 512, Duration::from_secs(120), 1024 * 1024);
            tiny_holds.push(h);
        }
        let large_old = budget.register();
        receiving_for(&large_old, 93_856, Duration::from_secs(90), 1024 * 1024);

        let waiter = budget.register();
        let _ = budget
            .reserve(&waiter, bulk(20_000), Duration::from_millis(300))
            .await;
        assert!(
            large_old.evicted().is_cancelled(),
            "old_candidates must keep evicting until `need` is satisfied"
        );
    }

    #[tokio::test]
    async fn impatient_bulk_waiters_do_not_starve_a_patient_small_waiter() {
        let budget = Arc::new(budget(1 << 20, 0));
        let large = budget.register();
        let _guard = budget.try_reserve(1 << 20, Class::Bulk).unwrap();
        large.charged(PoolKind::Bulk, 1 << 20, 256 * 1024);

        // Flood the slow reclaim bucket with impatient bulk waiters.
        let mut bulk_waiters = Vec::new();
        for _ in 0..8 {
            let budget = Arc::clone(&budget);
            bulk_waiters.push(tokio::spawn(async move {
                let hold = budget.register();
                let _ = budget
                    .reserve(&hold, bulk(1024), Duration::from_secs(2))
                    .await;
            }));
        }

        let small_waiter = budget.register();
        let _ = budget
            .reserve(&small_waiter, small(1024), Duration::from_millis(400))
            .await;
        assert!(
            large.evicted().is_cancelled(),
            "small waiter must still reclaim the large hold despite concurrent bulk waiters"
        );
        for task in bulk_waiters {
            task.abort();
        }
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
    fn ipv6_clients_are_limited_per_slash_64() {
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff:ffff:ffff:ffff".parse().unwrap();
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(ip_limit_key(a), ip_limit_key(b));
        assert_ne!(ip_limit_key(a), ip_limit_key(other));

        let mapped: IpAddr = "::ffff:192.0.2.7".parse().unwrap();
        assert_eq!(ip_limit_key(mapped), "192.0.2.7".parse::<IpAddr>().unwrap());

        let cfg = Config {
            per_ip_max_connections: 1,
            ..Config::default()
        };
        let capacity = Capacity::new(&cfg, Arc::new(Metrics::new()));
        let _held = capacity.try_acquire_ip(a).unwrap();
        assert!(capacity.try_acquire_ip(b).is_err());
        capacity.try_acquire_ip(other).unwrap();
    }

    #[test]
    fn tcp_slots_are_capped_per_ip_with_headroom() {
        assert_eq!(per_ip_tcp_limit(0), 0);
        assert_eq!(per_ip_tcp_limit(1), 9);
        assert_eq!(per_ip_tcp_limit(256), 320);

        let cfg = Config {
            per_ip_max_connections: 1,
            ..Config::default()
        };
        let capacity = Capacity::new(&cfg, Arc::new(Metrics::new()));
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let slots: Vec<_> = (0..9)
            .map(|_| capacity.try_acquire_tcp(ip).unwrap())
            .collect();
        assert_eq!(
            capacity.try_acquire_tcp(ip).unwrap_err(),
            TcpRejection::PerIp
        );
        // Another client is unaffected.
        capacity
            .try_acquire_tcp("192.0.2.2".parse().unwrap())
            .unwrap();
        drop(slots);
        capacity.try_acquire_tcp(ip).unwrap();
    }

    #[test]
    fn tcp_capacity_is_reported_before_the_per_ip_cap() {
        let cfg = Config {
            max_connections: 1,
            max_tcp_connections: 1,
            ..Config::default()
        };
        let capacity = Capacity::new(&cfg, Arc::new(Metrics::new()));
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let _slot = capacity.try_acquire_tcp(ip).unwrap();
        assert_eq!(
            capacity.try_acquire_tcp(ip).unwrap_err(),
            TcpRejection::Capacity
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
