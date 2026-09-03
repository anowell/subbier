//! The four load-balancing strategies. A strategy is a ranking and nothing else;
//! whether a request may hop accounts is stickiness and affinity, which
//! [`crate::balance::Router`] owns. Unknown usage sorts last under *both* usage
//! strategies: scoring it 100 would preferentially drain the least-known account.

use std::cmp::Ordering;

use crate::model::{StrategyKind, SubId};

/// A sub the router is willing to place this request on, with everything the
/// four strategies rank by.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub sub: SubId,
    /// Account-wide allowance consumed, `0..=100`, as `max(session, weekly)`.
    /// `None` means the usage fetch FAILED: it is not 100%, it never
    /// quarantines, and it sorts last under both usage strategies.
    pub usage_pct: Option<f32>,
    /// Proxy-observed, so a strict subset of the account's real concurrency.
    pub proxied_in_flight: u32,
    /// Proxy-observed requests routed here since the engine started.
    pub proxied_requests_total: u64,
}

impl Candidate {
    /// A failed fetch scores `100.0`. **Not the rank key**: it cannot tell
    /// "known to be full" from "we have no idea", so the strategies check
    /// [`Candidate::usage_known`] first.
    #[must_use]
    pub fn effective_pct(&self) -> f32 {
        self.usage_pct.unwrap_or(100.0)
    }

    #[must_use]
    pub const fn usage_known(&self) -> bool {
        self.usage_pct.is_some()
    }
}

/// Round-robin's cursor, one per provider. A [`SubId`] rather than an index:
/// the candidate set churns as subs exhaust and recover, and an index cursor
/// silently skips or repeats subs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StrategyState {
    pub last_picked: Option<SubId>,
}

pub trait Strategy: Send + Sync {
    /// Whether the router must pay for a usage round before calling
    /// [`Strategy::pick`].
    fn needs_usage(&self) -> bool {
        false
    }

    /// Rank `candidates` and return an index into it.
    ///
    /// `candidates` is never empty; the router guarantees it. Every
    /// implementation must be total and deterministic — ties break on
    /// [`SubId`] so two engines with identical inputs agree.
    fn pick(&self, candidates: &[Candidate], state: &mut StrategyState) -> usize;
}

#[must_use]
pub fn for_kind(kind: StrategyKind) -> &'static dyn Strategy {
    match kind {
        StrategyKind::LowestUsage => &LowestUsage,
        StrategyKind::HighestUsage => &HighestUsage,
        StrategyKind::RoundRobin => &RoundRobin,
        StrategyKind::LeastConnections => &LeastConnections,
    }
}

/// Fewest percent of the account-wide allowance used. The default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LowestUsage;

/// Most percent used, **among candidates below 100** — drain one account fully
/// before touching the next.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HighestUsage;

/// Rotate through candidates in [`SubId`] order, one per request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoundRobin;

/// Fewest proxy-observed in-flight requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LeastConnections;

fn known_first(c: &Candidate) -> u8 {
    u8::from(!c.usage_known())
}

/// `0` = a known figure below 100 (a real drain target), `1` = a failed fetch,
/// `2` = known to be full.
fn drain_tier(c: &Candidate) -> u8 {
    match c.usage_pct {
        Some(pct) if pct < 100.0 => 0,
        None => 1,
        Some(_) => 2,
    }
}

fn pick_min_by(
    candidates: &[Candidate],
    key: impl Fn(&Candidate, &Candidate) -> Ordering,
) -> usize {
    candidates
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| key(a, b))
        .map_or(0, |(index, _)| index)
}

impl Strategy for LowestUsage {
    fn needs_usage(&self) -> bool {
        true
    }

    fn pick(&self, candidates: &[Candidate], _state: &mut StrategyState) -> usize {
        pick_min_by(candidates, |a, b| {
            known_first(a)
                .cmp(&known_first(b))
                .then_with(|| a.effective_pct().total_cmp(&b.effective_pct()))
                .then_with(|| a.proxied_in_flight.cmp(&b.proxied_in_flight))
                .then_with(|| a.sub.cmp(&b.sub))
        })
    }
}

impl Strategy for HighestUsage {
    fn needs_usage(&self) -> bool {
        true
    }

    fn pick(&self, candidates: &[Candidate], _state: &mut StrategyState) -> usize {
        pick_min_by(candidates, |a, b| {
            drain_tier(a)
                .cmp(&drain_tier(b))
                // descending percentage: drain the fullest usable account first
                .then_with(|| b.effective_pct().total_cmp(&a.effective_pct()))
                .then_with(|| a.proxied_in_flight.cmp(&b.proxied_in_flight))
                .then_with(|| a.sub.cmp(&b.sub))
        })
    }
}

impl Strategy for RoundRobin {
    fn pick(&self, candidates: &[Candidate], state: &mut StrategyState) -> usize {
        let mut order: Vec<usize> = (0..candidates.len()).collect();
        order.sort_by_key(|&i| candidates[i].sub);

        // The first candidate strictly after the cursor, wrapping. Correct even
        // when the sub the cursor names has vanished from the candidate set.
        let index = state
            .last_picked
            .and_then(|last| order.iter().copied().find(|&i| candidates[i].sub > last))
            .or_else(|| order.first().copied())
            .unwrap_or(0);

        state.last_picked = candidates.get(index).map(|c| c.sub);
        index
    }
}

impl Strategy for LeastConnections {
    fn pick(&self, candidates: &[Candidate], _state: &mut StrategyState) -> usize {
        pick_min_by(candidates, |a, b| {
            a.proxied_in_flight
                .cmp(&b.proxied_in_flight)
                .then_with(|| a.proxied_requests_total.cmp(&b.proxied_requests_total))
                .then_with(|| a.sub.cmp(&b.sub))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `usage`: `Some` = a successful fetch, `None` = the fetch failed.
    fn c(id: u32, usage: Option<f32>, in_flight: u32, total: u64) -> Candidate {
        Candidate {
            sub: SubId(id),
            usage_pct: usage,
            proxied_in_flight: in_flight,
            proxied_requests_total: total,
        }
    }

    fn pick(kind: StrategyKind, candidates: &[Candidate]) -> SubId {
        let mut state = StrategyState::default();
        candidates[for_kind(kind).pick(candidates, &mut state)].sub
    }

    #[test]
    fn only_the_usage_strategies_make_the_router_pay_for_a_usage_round() {
        assert!(for_kind(StrategyKind::LowestUsage).needs_usage());
        assert!(for_kind(StrategyKind::HighestUsage).needs_usage());
        assert!(!for_kind(StrategyKind::RoundRobin).needs_usage());
        assert!(!for_kind(StrategyKind::LeastConnections).needs_usage());
    }

    #[test]
    fn strategies_rank_the_documented_way() {
        let cases: Vec<(&str, StrategyKind, Vec<Candidate>, u32)> = vec![
            (
                "lowest picks the least-used account",
                StrategyKind::LowestUsage,
                vec![
                    c(1, Some(100.0), 0, 0),
                    c(2, Some(40.0), 0, 0),
                    c(3, Some(10.0), 0, 0),
                ],
                3,
            ),
            (
                "lowest breaks ties on fewest proxied in-flight",
                StrategyKind::LowestUsage,
                vec![c(1, Some(10.0), 3, 0), c(2, Some(10.0), 1, 0)],
                2,
            ),
            (
                "lowest breaks remaining ties on SubId",
                StrategyKind::LowestUsage,
                vec![c(9, Some(10.0), 1, 0), c(4, Some(10.0), 1, 0)],
                4,
            ),
            (
                "highest drains the fullest account that is still below 100",
                StrategyKind::HighestUsage,
                vec![
                    c(1, Some(10.0), 0, 0),
                    c(2, Some(99.0), 0, 0),
                    c(3, Some(40.0), 0, 0),
                ],
                2,
            ),
            (
                "highest skips an account already known to be full",
                StrategyKind::HighestUsage,
                vec![c(1, Some(100.0), 0, 0), c(2, Some(40.0), 0, 0)],
                2,
            ),
            (
                "round-robin with no cursor starts at the lowest SubId",
                StrategyKind::RoundRobin,
                vec![c(7, None, 0, 0), c(2, None, 0, 0), c(5, None, 0, 0)],
                2,
            ),
            (
                "least-connections picks the fewest in-flight",
                StrategyKind::LeastConnections,
                vec![c(1, None, 4, 0), c(2, None, 1, 0), c(3, None, 9, 0)],
                2,
            ),
            (
                "least-connections breaks ties on fewest total requests",
                StrategyKind::LeastConnections,
                vec![c(1, None, 2, 900), c(2, None, 2, 10)],
                2,
            ),
            (
                "least-connections breaks remaining ties on SubId",
                StrategyKind::LeastConnections,
                vec![c(8, None, 0, 0), c(3, None, 0, 0)],
                3,
            ),
            (
                "any strategy handles a single candidate",
                StrategyKind::HighestUsage,
                vec![c(42, None, 0, 0)],
                42,
            ),
        ];

        for (name, kind, candidates, expected) in cases {
            assert_eq!(pick(kind, &candidates), SubId(expected), "{name}");
        }
    }

    #[test]
    fn unknown_usage_sorts_last_under_lowest_usage() {
        let candidates = vec![c(1, None, 0, 0), c(2, Some(90.0), 0, 0)];
        assert_eq!(pick(StrategyKind::LowestUsage, &candidates), SubId(2));

        // "Known to be full" and "we have no idea" must not collapse together.
        let candidates = vec![c(1, None, 0, 0), c(2, Some(100.0), 0, 0)];
        assert_eq!(pick(StrategyKind::LowestUsage, &candidates), SubId(2));

        // With nothing but failed fetches it is still total: lowest SubId.
        let candidates = vec![c(5, None, 0, 0), c(3, None, 0, 0)];
        assert_eq!(pick(StrategyKind::LowestUsage, &candidates), SubId(3));
    }

    #[test]
    fn unknown_usage_sorts_last_under_highest_usage_too() {
        // Taking the unknown would drain the account we know least about.
        let candidates = vec![c(1, None, 0, 0), c(2, Some(1.0), 0, 0)];
        assert_eq!(pick(StrategyKind::HighestUsage, &candidates), SubId(2));

        let candidates = vec![c(1, None, 0, 0), c(2, Some(99.9), 0, 0)];
        assert_eq!(pick(StrategyKind::HighestUsage, &candidates), SubId(2));

        // An unknown does beat an account known to be full: it is the only one
        // that might still work.
        let candidates = vec![c(1, Some(100.0), 0, 0), c(2, None, 0, 0)];
        assert_eq!(pick(StrategyKind::HighestUsage, &candidates), SubId(2));

        let candidates = vec![c(5, None, 0, 0), c(3, None, 0, 0)];
        assert_eq!(pick(StrategyKind::HighestUsage, &candidates), SubId(3));
    }

    #[test]
    fn round_robin_visits_every_candidate_once_per_cycle() {
        let candidates = vec![c(3, None, 0, 0), c(1, None, 0, 0), c(2, None, 0, 0)];
        let mut state = StrategyState::default();
        let rr = for_kind(StrategyKind::RoundRobin);

        let visited: Vec<u32> = (0..6)
            .map(|_| candidates[rr.pick(&candidates, &mut state)].sub.0)
            .collect();
        assert_eq!(visited, vec![1, 2, 3, 1, 2, 3]);
    }

    #[test]
    fn round_robin_survives_a_sub_leaving_and_rejoining_mid_cycle() {
        let all = vec![c(1, None, 0, 0), c(2, None, 0, 0), c(3, None, 0, 0)];
        let without_two = vec![c(1, None, 0, 0), c(3, None, 0, 0)];
        let rr = for_kind(StrategyKind::RoundRobin);
        let mut state = StrategyState::default();

        assert_eq!(all[rr.pick(&all, &mut state)].sub, SubId(1));

        // 2 exhausts. An index cursor would have handed back 3's slot as 2's.
        assert_eq!(without_two[rr.pick(&without_two, &mut state)].sub, SubId(3));
        assert_eq!(without_two[rr.pick(&without_two, &mut state)].sub, SubId(1));

        // 2 recovers, and is not skipped.
        assert_eq!(all[rr.pick(&all, &mut state)].sub, SubId(2));
        assert_eq!(all[rr.pick(&all, &mut state)].sub, SubId(3));

        // A cursor naming a sub that is gone entirely wraps to the first.
        let mut state = StrategyState {
            last_picked: Some(SubId(9)),
        };
        assert_eq!(all[rr.pick(&all, &mut state)].sub, SubId(1));
    }
}
