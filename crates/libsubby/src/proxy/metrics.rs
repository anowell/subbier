//! In-flight gauges and the proxied-token ring buffer. Every counter measures
//! only traffic subbier routed, hence the `proxied_` prefix on every accessor.
//! Each request is counted twice — against the sub, and against the endpoint —
//! so a pool's numbers cannot be summed from members who sit in other pools.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use jiff::{SignedDuration, Timestamp};

use crate::model::SubId;

const TOKEN_WINDOW: SignedDuration = SignedDuration::from_hours(1);

/// Hard cap per counter, so a pathological request rate cannot grow the ring
/// without bound between expiries.
const MAX_TICKS: usize = 8192;

/// `last_proxied_at` sentinel for "never".
const NEVER: i64 = i64::MIN;

#[derive(Debug, Clone, Copy)]
struct TokenTick {
    at: Timestamp,
    input: u64,
    output: u64,
}

/// One population's counters: a sub, or a proxy endpoint.
#[derive(Debug, Default)]
struct Counters {
    in_flight: AtomicU32,
    requests_total: AtomicU64,
    /// Microseconds since the epoch, or [`NEVER`].
    last_proxied_at: AtomicI64,
    tokens: Mutex<Vec<TokenTick>>,
}

impl Counters {
    fn new() -> Self {
        Self {
            last_proxied_at: AtomicI64::new(NEVER),
            ..Self::default()
        }
    }

    /// Reading is also when we expire, so a counter that stopped seeing traffic
    /// falls to zero rather than holding its last hour forever.
    fn tokens_since(&self, cutoff: Timestamp, now: Timestamp) -> u64 {
        let mut ticks = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        ticks.retain(|tick| tick.at > cutoff);
        ticks
            .iter()
            .filter(|tick| tick.at <= now)
            .map(|tick| tick.input.saturating_add(tick.output))
            .fold(0u64, u64::saturating_add)
    }
}

/// Proxy-observed counters, kept per sub and per proxy endpoint. Read the module
/// docs before rendering any of these next to an allowance percentage.
#[derive(Debug, Default)]
pub struct Metrics {
    subs: RwLock<HashMap<SubId, Arc<Counters>>>,
    /// Keyed by the pool whose URL served the request. The bare proxy has no
    /// entry: its totals are summed out of `subs` instead.
    pools: RwLock<HashMap<String, Arc<Counters>>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start counting one in-flight request against `sub`, and against `pool`
    /// when it came in on a `/pool/<name>` URL. Hold the guard for the whole
    /// upstream request, streaming body included: dropping it at the response
    /// headers undercounts exactly the long requests balancing needs to see.
    pub fn in_flight(&self, sub: SubId, pool: Option<&str>) -> InFlightGuard {
        let now = Timestamp::now().as_microsecond();
        let counters: Vec<Arc<Counters>> = std::iter::once(self.entry(sub))
            .chain(pool.map(|name| self.pool_entry(name)))
            .collect();
        for entry in &counters {
            entry.in_flight.fetch_add(1, Ordering::Relaxed);
            entry.requests_total.fetch_add(1, Ordering::Relaxed);
            entry.last_proxied_at.store(now, Ordering::Relaxed);
        }
        InFlightGuard { counters }
    }

    /// Against the same two populations [`Metrics::in_flight`] counted it in.
    pub fn record_tokens(
        &self,
        sub: SubId,
        pool: Option<&str>,
        input: u64,
        output: u64,
        at: Timestamp,
    ) {
        let cutoff = at.checked_sub(TOKEN_WINDOW).unwrap_or(Timestamp::MIN);
        for entry in std::iter::once(self.entry(sub)).chain(pool.map(|name| self.pool_entry(name)))
        {
            let mut ticks = entry.tokens.lock().unwrap_or_else(|e| e.into_inner());
            ticks.retain(|tick| tick.at > cutoff);
            ticks.push(TokenTick { at, input, output });
            if ticks.len() > MAX_TICKS {
                let overflow = ticks.len() - MAX_TICKS;
                ticks.drain(..overflow);
            }
        }
    }

    pub fn proxied_in_flight(&self, sub: SubId) -> u32 {
        self.get(sub)
            .map_or(0, |entry| entry.in_flight.load(Ordering::Relaxed))
    }

    pub fn proxied_requests_total(&self, sub: SubId) -> u64 {
        self.get(sub)
            .map_or(0, |entry| entry.requests_total.load(Ordering::Relaxed))
    }

    pub fn proxied_tokens_1h(&self, sub: SubId, now: Timestamp) -> u64 {
        let cutoff = now.checked_sub(TOKEN_WINDOW).unwrap_or(Timestamp::MIN);
        self.get(sub)
            .map_or(0, |entry| entry.tokens_since(cutoff, now))
    }

    pub fn last_proxied_at(&self, sub: SubId) -> Option<Timestamp> {
        let micros = self.get(sub)?.last_proxied_at.load(Ordering::Relaxed);
        (micros != NEVER)
            .then(|| Timestamp::from_microsecond(micros).ok())
            .flatten()
    }

    pub fn proxied_counters(&self, sub: SubId) -> ProxiedCounters {
        ProxiedCounters {
            proxied_in_flight: self.proxied_in_flight(sub),
            proxied_requests_total: self.proxied_requests_total(sub),
            last_proxied_at: self.last_proxied_at(sub),
        }
    }

    /// One pool endpoint's own count, never the sum of its members'.
    pub fn pool_proxied_in_flight(&self, pool: &str) -> u32 {
        self.pool_get(pool)
            .map_or(0, |entry| entry.in_flight.load(Ordering::Relaxed))
    }

    /// One pool endpoint's own tokens, never the sum of its members'.
    pub fn pool_proxied_tokens_1h(&self, pool: &str, now: Timestamp) -> u64 {
        let cutoff = now.checked_sub(TOKEN_WINDOW).unwrap_or(Timestamp::MIN);
        self.pool_get(pool)
            .map_or(0, |entry| entry.tokens_since(cutoff, now))
    }

    pub fn total_proxied_in_flight(&self) -> u32 {
        self.read()
            .values()
            .map(|entry| entry.in_flight.load(Ordering::Relaxed))
            .fold(0u32, u32::saturating_add)
    }

    pub fn total_proxied_requests(&self) -> u64 {
        self.read()
            .values()
            .map(|entry| entry.requests_total.load(Ordering::Relaxed))
            .fold(0u64, u64::saturating_add)
    }

    pub fn total_proxied_tokens_1h(&self, now: Timestamp) -> u64 {
        let cutoff = now.checked_sub(TOKEN_WINDOW).unwrap_or(Timestamp::MIN);
        self.read()
            .values()
            .map(|entry| entry.tokens_since(cutoff, now))
            .fold(0u64, u64::saturating_add)
    }

    /// Drop everything remembered about `sub`. Pool counters are untouched:
    /// what an endpoint carried stays true after a member leaves.
    pub fn forget(&self, sub: SubId) {
        self.write().remove(&sub);
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<SubId, Arc<Counters>>> {
        self.subs.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<SubId, Arc<Counters>>> {
        self.subs.write().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, sub: SubId) -> Option<Arc<Counters>> {
        self.read().get(&sub).cloned()
    }

    fn entry(&self, sub: SubId) -> Arc<Counters> {
        if let Some(entry) = self.get(sub) {
            return entry;
        }
        Arc::clone(
            self.write()
                .entry(sub)
                .or_insert_with(|| Arc::new(Counters::new())),
        )
    }

    fn pool_get(&self, pool: &str) -> Option<Arc<Counters>> {
        self.pools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(pool)
            .cloned()
    }

    fn pool_entry(&self, pool: &str) -> Arc<Counters> {
        if let Some(entry) = self.pool_get(pool) {
            return entry;
        }
        Arc::clone(
            self.pools
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .entry(pool.to_owned())
                .or_insert_with(|| Arc::new(Counters::new())),
        )
    }
}

/// Field names match `snapshot::RoutingView` so the engine can copy them across.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProxiedCounters {
    pub proxied_in_flight: u32,
    pub proxied_requests_total: u64,
    pub last_proxied_at: Option<Timestamp>,
}

/// RAII in-flight counter. Decrements on drop, unwind included, and holds every
/// counter the request was opened against so they fall together.
#[derive(Debug)]
pub struct InFlightGuard {
    counters: Vec<Arc<Counters>>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        for entry in &self.counters {
            // Saturate rather than wrap: an underflow would make
            // `LeastConnections` avoid a perfectly healthy sub forever.
            let _ = entry
                .in_flight
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    Some(n.saturating_sub(1))
                });
        }
    }
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Metrics>();
    assert_send_sync::<InFlightGuard>();
};

#[cfg(test)]
mod tests {
    use super::*;

    const A: SubId = SubId(1);
    const B: SubId = SubId(2);

    fn now() -> Timestamp {
        "2026-08-26T12:00:00Z".parse().unwrap()
    }

    fn minutes_before(now: Timestamp, minutes: i64) -> Timestamp {
        now.checked_sub(SignedDuration::from_mins(minutes)).unwrap()
    }

    #[test]
    fn the_guard_increments_and_decrements() {
        let metrics = Metrics::new();
        assert_eq!(metrics.proxied_in_flight(A), 0);
        assert_eq!(metrics.last_proxied_at(A), None);

        let first = metrics.in_flight(A, None);
        let second = metrics.in_flight(A, None);
        let stamped = metrics
            .last_proxied_at(A)
            .expect("stamped when routing starts");
        assert_eq!(metrics.proxied_in_flight(A), 2);
        assert_eq!(metrics.proxied_in_flight(B), 0);
        assert_eq!(metrics.total_proxied_in_flight(), 2);

        drop(first);
        assert_eq!(metrics.proxied_in_flight(A), 1);
        drop(second);
        assert_eq!(
            metrics.proxied_counters(A),
            ProxiedCounters {
                proxied_in_flight: 0,
                // Cumulative, even though nothing is in flight.
                proxied_requests_total: 2,
                last_proxied_at: Some(stamped),
            }
        );
        assert_eq!(metrics.total_proxied_requests(), 2);
    }

    #[test]
    fn the_guard_decrements_on_a_panic_unwind() {
        let metrics = Arc::new(Metrics::new());
        let held = metrics.in_flight(A, None);

        let result = std::panic::catch_unwind({
            let metrics = Arc::clone(&metrics);
            move || {
                let _guard = metrics.in_flight(A, None);
                assert_eq!(metrics.proxied_in_flight(A), 2);
                panic!("upstream exploded");
            }
        });

        assert!(result.is_err());
        assert_eq!(
            metrics.proxied_in_flight(A),
            1,
            "the panicking guard unwound"
        );
        drop(held);
        assert_eq!(metrics.proxied_in_flight(A), 0);
        assert_eq!(metrics.proxied_requests_total(A), 2);
    }

    #[test]
    fn an_unobserved_sub_reads_as_zero_rather_than_absent() {
        let metrics = Metrics::new();
        assert_eq!(metrics.proxied_in_flight(B), 0);
        assert_eq!(metrics.proxied_requests_total(B), 0);
        assert_eq!(metrics.proxied_tokens_1h(B, now()), 0);
        assert_eq!(metrics.last_proxied_at(B), None);
        assert_eq!(metrics.proxied_counters(B), ProxiedCounters::default());
    }

    #[test]
    fn the_token_ring_sums_the_hour_and_expires_the_rest() {
        let metrics = Metrics::new();
        let now = now();
        metrics.record_tokens(A, None, 1_000, 1_000, minutes_before(now, 90));
        metrics.record_tokens(A, None, 100, 20, minutes_before(now, 30));
        metrics.record_tokens(A, None, 5, 1, minutes_before(now, 1));
        metrics.record_tokens(B, None, 999, 999, minutes_before(now, 1));
        assert_eq!(metrics.proxied_tokens_1h(A, now), 126);
        assert_eq!(metrics.proxied_tokens_1h(B, now), 1998);

        let later = now.checked_add(SignedDuration::from_hours(1)).unwrap();
        assert_eq!(metrics.proxied_tokens_1h(A, later), 0);
    }

    #[test]
    fn forgetting_a_sub_spares_live_guards_and_the_pools_own_record() {
        let metrics = Metrics::new();
        let now = now();
        let guard = metrics.in_flight(A, Some("moonshot"));
        metrics.record_tokens(A, Some("moonshot"), 10, 5, now);
        metrics.forget(A);
        assert_eq!(metrics.proxied_in_flight(A), 0);
        assert_eq!(metrics.proxied_tokens_1h(A, now), 0);
        assert_eq!(metrics.pool_proxied_tokens_1h("moonshot", now), 15);
        drop(guard);
        assert_eq!(metrics.proxied_in_flight(A), 0);
    }

    #[test]
    fn concurrent_guards_and_ticks_agree() {
        let metrics = Arc::new(Metrics::new());
        let now = now();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let metrics = Arc::clone(&metrics);
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        let _guard = metrics.in_flight(A, None);
                        metrics.record_tokens(A, None, 1, 1, now);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(metrics.proxied_in_flight(A), 0);
        assert_eq!(metrics.proxied_requests_total(A), 800);
        assert_eq!(metrics.proxied_tokens_1h(A, now), 1600);
    }

    #[test]
    fn a_pools_counters_are_its_endpoints_own() {
        let metrics = Metrics::new();
        let now = now();

        // A goes out through /pool/moonshot, B through the bare proxy.
        let held = metrics.in_flight(A, Some("moonshot"));
        metrics.record_tokens(A, Some("moonshot"), 10, 5, now);
        metrics.record_tokens(B, None, 1_000, 1_000, now);

        assert_eq!(metrics.pool_proxied_in_flight("moonshot"), 1);
        assert_eq!(metrics.pool_proxied_tokens_1h("moonshot", now), 15);

        assert_eq!(metrics.pool_proxied_in_flight("other"), 0);
        assert_eq!(metrics.pool_proxied_tokens_1h("other", now), 0);

        // The proxy's own totals are everything, whichever URL it came in on.
        assert_eq!(metrics.total_proxied_in_flight(), 1);
        assert_eq!(metrics.total_proxied_tokens_1h(now), 2015);

        drop(held);
        assert_eq!(metrics.pool_proxied_in_flight("moonshot"), 0);
        assert_eq!(metrics.total_proxied_in_flight(), 0);
    }

    #[test]
    fn one_sub_in_two_pools_counts_in_each() {
        let metrics = Metrics::new();
        let first = metrics.in_flight(A, Some("moonshot"));
        let second = metrics.in_flight(A, Some("critical"));
        assert_eq!(metrics.pool_proxied_in_flight("moonshot"), 1);
        assert_eq!(metrics.pool_proxied_in_flight("critical"), 1);
        assert_eq!(metrics.proxied_in_flight(A), 2);
        assert_eq!(metrics.total_proxied_in_flight(), 2);
        drop(first);
        drop(second);
        assert_eq!(metrics.pool_proxied_in_flight("moonshot"), 0);
        assert_eq!(metrics.pool_proxied_in_flight("critical"), 0);
    }
}
