//! Router: which sub serves each proxied request — pin, chain hint, cache-key
//! hint, stickiness, strategy, in that order, each soft. The chain outranks the
//! key: moving a chain loses account-scoped Codex reasoning items, a key only
//! cache warmth. Stickiness resolves before the usage round, not per request.

pub mod strategy;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};

use crate::model::{Provider, StrategyKind, SubId, Usage};

pub use strategy::{Candidate, Strategy, StrategyState};

pub const DEFAULT_USAGE_DEADLINE: Duration = Duration::from_secs(5);

/// How long a sub is quarantined when we have no reset information at all.
pub const BLIND_EXHAUSTION: SignedDuration = SignedDuration::from_secs(5 * 60);

/// The substrings that make a 429 a real *plan exhausted* error rather than an
/// ordinary rate limit, matched case-insensitively.
pub const USAGE_LIMIT_MARKERS: [&str; 7] = [
    "GoUsageLimitError",
    "FreeUsageLimitError",
    "Monthly usage limit reached",
    "usage limit",
    "usage_limit",
    "out of budget",
    "quota exceeded",
];

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
    match needle.len() {
        0 => true,
        n if n > haystack.len() => false,
        n => haystack.windows(n).any(|w| w.eq_ignore_ascii_case(needle)),
    }
}

#[must_use]
pub fn is_usage_limit_body(body: &str) -> bool {
    USAGE_LIMIT_MARKERS
        .iter()
        .any(|marker| contains_ignore_ascii_case(body, marker))
}

/// A plain 429 is not one: rotating on an ordinary rate limit would spread a
/// burst across every account instead of slowing it down.
#[must_use]
pub fn is_usage_limit_error(status: u16, body: &str) -> bool {
    status == 429 && is_usage_limit_body(body)
}

/// One upstream failure, as the proxy observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attempt<'a> {
    pub status: u16,
    /// The response body, or a truncated prefix of it. Never a token.
    pub body: &'a str,
    /// Set when the proxy knows no other sub could accept the same bytes; it
    /// suppresses rotation outright. A flag rather than a body heuristic because
    /// Anthropic answers a malformed `system` array with a real-looking 429.
    pub request_scoped: bool,
    /// The first 401 retries the same sub; the second is permanent.
    pub retried_auth: bool,
}

impl<'a> Attempt<'a> {
    #[must_use]
    pub const fn new(status: u16, body: &'a str) -> Self {
        Self {
            status,
            body,
            request_scoped: false,
            retried_auth: false,
        }
    }

    #[must_use]
    pub const fn request_scoped(mut self) -> Self {
        self.request_scoped = true;
        self
    }

    #[must_use]
    pub const fn after_auth_retry(mut self) -> Self {
        self.retried_auth = true;
        self
    }

    /// [`Attempt::request_scoped`] wins over every status-based rule: a request
    /// the upstream refuses on its own merits is no reason to try another account.
    #[must_use]
    pub fn classify(&self) -> FailureClass {
        if self.request_scoped {
            return FailureClass::RequestScoped;
        }
        match self.status {
            429 if is_usage_limit_body(self.body) => FailureClass::UsageLimit,
            429 => FailureClass::RateLimited,
            401 if self.retried_auth => FailureClass::AuthPermanent,
            401 => FailureClass::AuthRetryable,
            _ => FailureClass::Passthrough,
        }
    }
}

/// What the router does with a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailureClass {
    /// A 429 matching [`USAGE_LIMIT_MARKERS`]: rotate and quarantine.
    UsageLimit,
    /// No other credential can accept this body, so rotating would burn one
    /// account per candidate on one bad request.
    RequestScoped,
    /// An ordinary 429. Pass it through; the client backs off itself.
    RateLimited,
    /// A 401 not yet retried: force a refresh and retry the same sub once.
    AuthRetryable,
    /// A 401 that survived the retry. Rotate and quarantine.
    AuthPermanent,
    /// Refresh failed on network, timeout or 5xx — not the account's fault: no
    /// quarantine, no rotation, 502 to the client.
    RefreshTransient,
    /// 400/401/403 from the token endpoint: the refresh token is dead.
    RefreshPermanent,
    /// Anything else. Pass it through untouched.
    Passthrough,
}

impl FailureClass {
    #[must_use]
    pub const fn quarantines(self) -> bool {
        matches!(
            self,
            FailureClass::UsageLimit | FailureClass::AuthPermanent | FailureClass::RefreshPermanent
        )
    }

    /// Ignores `auto_switch`; [`Router::on_failure`] applies it for you.
    #[must_use]
    pub const fn disposition(self) -> Disposition {
        match self {
            FailureClass::UsageLimit
            | FailureClass::AuthPermanent
            | FailureClass::RefreshPermanent => Disposition::Rotate,
            FailureClass::AuthRetryable => Disposition::RetrySameSub,
            FailureClass::RefreshTransient => Disposition::Fail { status: 502 },
            FailureClass::RequestScoped | FailureClass::RateLimited | FailureClass::Passthrough => {
                Disposition::PassThrough
            }
        }
    }
}

/// The action the proxy takes after an [`Attempt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Disposition {
    /// Hand the upstream response back to the client unchanged.
    PassThrough,
    /// Force a token refresh and retry the **same** sub, once.
    RetrySameSub,
    /// Try the next candidate, up to `Selection::candidates` attempts.
    Rotate,
    /// Stop now and answer with `status`. Do not touch another sub.
    Fail { status: u16 },
}

/// `status` is the token endpoint's status, `None` a network error. Only
/// 400/401/403 mean a dead refresh token; a 503 is a bad day upstream.
#[must_use]
pub const fn classify_refresh(status: Option<u16>) -> FailureClass {
    match status {
        Some(400 | 401 | 403) => FailureClass::RefreshPermanent,
        _ => FailureClass::RefreshTransient,
    }
}

/// The worse of the two headline windows. Scoped windows are excluded: a
/// model-specific cap says nothing about a request for a different model.
#[must_use]
pub fn effective_pct(usage: &Usage) -> f32 {
    let session = usage.session.map_or(0.0, |w| w.pct);
    let weekly = usage.weekly.map_or(0.0, |w| w.pct);
    session.max(weekly)
}

/// The provider's verdict first, the percentage as fallback: the usage endpoint
/// lags the enforcement decision, and an account can be cut off below 100%.
/// `limit_reached: Some(false)` is a floor, not a veto.
#[must_use]
pub fn is_exhausted(usage: &Usage) -> bool {
    usage.limit_reached == Some(true) || effective_pct(usage) >= 100.0
}

/// When a sub quarantined at `now` should be reconsidered, over the windows
/// that reset in the future: the *latest* reset among the full ones (every full
/// window must roll over), else the *earliest* reset (usage has not caught up,
/// and waiting a week would be absurd), else [`BLIND_EXHAUSTION`].
#[must_use]
pub fn exhaust_until(now: Timestamp, usage: Option<&Usage>) -> Timestamp {
    let windows: Vec<(f32, Timestamp)> = usage
        .into_iter()
        .flat_map(|u| [u.session, u.weekly])
        .flatten()
        .filter_map(|w| w.resets_at.map(|at| (w.pct, at)))
        .filter(|(_, at)| *at > now)
        .collect();

    let full = windows
        .iter()
        .filter(|(pct, _)| *pct >= 100.0)
        .map(|(_, at)| *at)
        .max();

    full.or_else(|| windows.iter().map(|(_, at)| *at).min())
        .unwrap_or(now + BLIND_EXHAUSTION)
}

pub type UsageRound<'a> = Pin<Box<dyn Future<Output = Vec<Option<Usage>>> + Send + 'a>>;

/// How the router gets usage figures, injected so the router never does I/O.
pub trait UsageScorer: Send + Sync {
    /// `deadline` is a round-level budget, not a per-request one, so one hung
    /// account cannot stall the round. The result is parallel to `subs`; `None`
    /// means the fetch failed, which is not 100% and must never quarantine.
    /// The result is parallel to `subs`; `None` (or a missing entry) means the
    /// fetch failed, which is not 100% and must never quarantine.
    fn usage_round<'a>(&'a self, subs: &'a [SubId], deadline: Duration) -> UsageRound<'a>;
}

/// Everything the router needs to know about one sub, per request. The
/// `proxied_*` counters are a strict subset of the account's real traffic.
#[derive(Debug, Clone, PartialEq)]
pub struct SubStatus {
    pub sub: SubId,
    pub provider: Provider,
    pub enabled: bool,
    /// Skipped indefinitely: no amount of waiting fixes a dead refresh token.
    pub needs_login: bool,
    pub proxied_in_flight: u32,
    pub proxied_requests_total: u64,
    /// The account-wide session allowance, last time we saw it, `0..=100`.
    /// `None` means never fetched, which [`PoolGate::admits`] never excludes.
    pub session_pct: Option<f32>,
    /// The account-wide weekly allowance. See [`SubStatus::session_pct`].
    pub weekly_pct: Option<f32>,
}

impl SubStatus {
    #[must_use]
    pub const fn new(sub: SubId, provider: Provider) -> Self {
        Self {
            sub,
            provider,
            enabled: true,
            needs_login: false,
            proxied_in_flight: 0,
            proxied_requests_total: 0,
            session_pct: None,
            weekly_pct: None,
        }
    }

    #[must_use]
    pub const fn with_proxied(mut self, in_flight: u32, total: u64) -> Self {
        self.proxied_in_flight = in_flight;
        self.proxied_requests_total = total;
        self
    }

    fn candidate(&self, usage_pct: Option<f32>) -> Candidate {
        Candidate {
            sub: self.sub,
            usage_pct,
            proxied_in_flight: self.proxied_in_flight,
            proxied_requests_total: self.proxied_requests_total,
        }
    }
}

/// Where a request would rather run, strongest first. Each is soft: an
/// unusable hint falls through, never to an error.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Hints {
    /// The sub that served the previous turn of this conversation.
    pub chain: Option<SubId>,
    /// The sub this `prompt_cache_key` was last placed on.
    pub key: Option<SubId>,
}

/// Why the router chose the sub it chose. For logs only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SelectReason {
    Pinned,
    /// `auto_switch` is off and this sub was already current.
    Frozen,
    /// The previous turn of this conversation ran here.
    Affinity,
    /// This `prompt_cache_key` was last placed here.
    Placement,
    /// Sticky, and this sub was already current. No usage was fetched.
    Sticky,
    Strategy(StrategyKind),
}

/// Who is in a pool, and how full a member may be before the pool stops using
/// it. A filter on the candidate set, never a re-ranking, so a pool composes
/// with every strategy, reading the provider's account-wide percentage.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolGate {
    /// For error messages.
    pub name: String,
    /// `None` means every sub, which is what a pool that only sets a ceiling
    /// looks like.
    pub members: Option<BTreeSet<SubId>>,
    /// Exclude a member whose session allowance is **at or above** this
    /// percentage (`0..=100`). `100.0` never excludes.
    pub max_session_pct: f32,
    /// Exclude a member whose weekly allowance is at or above this percentage.
    pub max_weekly_pct: f32,
}

impl PoolGate {
    #[must_use]
    pub fn contains(&self, sub: SubId) -> bool {
        self.members.as_ref().is_none_or(|m| m.contains(&sub))
    }

    /// An unknown percentage never excludes: refusing to route on missing data
    /// would turn a cold start into a dead pool.
    #[must_use]
    pub fn admits(&self, status: &SubStatus) -> bool {
        self.contains(status.sub)
            && status.session_pct.is_none_or(|p| p < self.max_session_pct)
            && status.weekly_pct.is_none_or(|p| p < self.max_weekly_pct)
    }

    /// Tells "no members" from "members all too full" — different problems.
    #[must_use]
    pub fn has_ceiling(&self) -> bool {
        self.max_session_pct < 100.0 || self.max_weekly_pct < 100.0
    }
}

/// The router's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub sub: SubId,
    pub reason: SelectReason,
    /// How many candidates were in play; the proxy retries at most this many
    /// times before giving up.
    pub candidates: usize,
}

/// Why the router could not pick anything.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SelectError {
    /// The user turned this provider off in the proxy settings.
    #[error("{provider} is not being proxied")]
    NotProxied { provider: Provider },
    /// No sub for this provider is configured, enabled and logged in.
    #[error("no usable {provider} subscription")]
    NoCandidates { provider: Provider },
    /// The named pool exists but admits nothing right now. With `over_ceiling`
    /// it is working as configured: holding back so another pool keeps headroom.
    #[error("{}", pool_message(pool, *over_ceiling))]
    PoolEmpty {
        pool: String,
        /// `true` when the pool has members and every one is at or above a
        /// ceiling; `false` when it has no usable members at all.
        over_ceiling: bool,
    },
    /// The URL named a pool that is not in `config.kdl`.
    #[error("unknown pool {pool:?}: no `pool \"{pool}\"` block in config.kdl")]
    UnknownPool { pool: String },
    /// Every sub for this provider is quarantined.
    #[error("all {provider} subs are used up")]
    AllExhausted {
        provider: Provider,
        /// The soonest quarantine to lift, if any is known.
        next_reset: Option<Timestamp>,
    },
}

impl SelectError {
    /// 429 only for a real exhaustion, so a misconfiguration is never mistaken
    /// for a rate limit.
    #[must_use]
    pub const fn status(&self) -> u16 {
        match self {
            SelectError::NotProxied { .. } | SelectError::NoCandidates { .. } => 503,
            // A pool holding its members back is a quota decision, so backoff
            // is the right client behaviour; an empty pool is a config mistake.
            SelectError::PoolEmpty { over_ceiling, .. } => {
                if *over_ceiling {
                    429
                } else {
                    503
                }
            }
            SelectError::UnknownPool { .. } => 404,
            SelectError::AllExhausted { .. } => 429,
        }
    }
}

fn pool_message(pool: &str, over_ceiling: bool) -> String {
    if over_ceiling {
        format!(
            "every subscription in pool {pool:?} is above the ceiling that pool sets; \
             its headroom is reserved for other pools"
        )
    } else {
        format!("pool {pool:?} has no usable subscription")
    }
}

/// The router's half of the settings. Mirrors `config.kdl`'s `proxy` node.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouterSettings {
    pub strategy: StrategyKind,
    /// `None` means "use [`StrategyKind::default_sticky`]", kept as an `Option`
    /// so an unset config stays unset when the strategy changes.
    pub sticky: Option<bool>,
    /// `false` freezes the proxy onto one sub: a request never silently
    /// changes identity, and a failure is returned rather than retried.
    pub auto_switch: bool,
    /// Indexed by [`Provider::index`].
    pub providers_proxied: [bool; 2],
    /// The usage round's shared deadline.
    pub usage_deadline: Duration,
}

impl Default for RouterSettings {
    fn default() -> Self {
        Self {
            strategy: StrategyKind::default(),
            sticky: None,
            auto_switch: true,
            providers_proxied: [true; 2],
            usage_deadline: DEFAULT_USAGE_DEADLINE,
        }
    }
}

impl RouterSettings {
    /// The user's override, else the strategy's default.
    #[must_use]
    pub fn sticky(&self) -> bool {
        self.sticky
            .unwrap_or_else(|| self.strategy.default_sticky())
    }

    #[must_use]
    pub const fn proxies(&self, provider: Provider) -> bool {
        self.providers_proxied[provider.index()]
    }
}

#[derive(Debug)]
struct State {
    settings: RouterSettings,
    pinned: Option<SubId>,
    /// The sub stickiness and `auto_switch` hold onto, per provider.
    current: [Option<SubId>; 2],
    /// Round-robin's cursor, per provider.
    strategies: [StrategyState; 2],
    /// In memory only: a restart re-probes rather than resurrecting a stale
    /// opinion about an account.
    exhausted: BTreeMap<SubId, Timestamp>,
}

impl State {
    fn sweep(&mut self, now: Timestamp) {
        self.exhausted.retain(|_, until| *until > now);
    }

    fn exhaust(&mut self, sub: SubId, usage: Option<&Usage>, now: Timestamp) -> Timestamp {
        let until = exhaust_until(now, usage);
        self.exhausted.insert(sub, until);
        for current in &mut self.current {
            if *current == Some(sub) {
                *current = None;
            }
        }
        until
    }
}

/// The internal lock is never held across an `.await`, so a hung usage round
/// cannot block an unrelated request from being routed.
#[derive(Debug)]
pub struct Router {
    state: Mutex<State>,
}

impl Default for Router {
    fn default() -> Self {
        Self::new(RouterSettings::default())
    }
}

impl Router {
    #[must_use]
    pub fn new(settings: RouterSettings) -> Self {
        Self {
            state: Mutex::new(State {
                settings,
                pinned: None,
                current: [None; 2],
                strategies: [StrategyState::default(); 2],
                exhausted: BTreeMap::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // The state is a set of hints, not an invariant worth a crash for.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[must_use]
    pub fn settings(&self) -> RouterSettings {
        self.lock().settings
    }

    /// Changing the strategy resets round-robin's cursor but keeps `current`,
    /// so a live conversation does not move because the user opened the menu.
    pub fn set_settings(&self, settings: RouterSettings) {
        let mut state = self.lock();
        if state.settings.strategy != settings.strategy {
            state.strategies = [StrategyState::default(); 2];
        }
        state.settings = settings;
    }

    /// Force every subsequent request onto `sub`. `None` clears the pin.
    pub fn pin(&self, sub: Option<SubId>) {
        self.lock().pinned = sub;
    }

    #[must_use]
    pub fn pinned(&self) -> Option<SubId> {
        self.lock().pinned
    }

    /// The sub stickiness is holding for `provider`.
    #[must_use]
    pub fn current(&self, provider: Provider) -> Option<SubId> {
        self.lock().current[provider.index()]
    }

    /// Forces the next request to re-rank.
    pub fn clear_current(&self, provider: Provider) {
        self.lock().current[provider.index()] = None;
    }

    /// Quarantine `sub` and clear it from `current`, returning its reset.
    pub fn exhaust(&self, sub: SubId, usage: Option<&Usage>) -> Timestamp {
        self.lock().exhaust(sub, usage, Timestamp::now())
    }

    /// `None` if it is not quarantined.
    #[must_use]
    pub fn exhausted_until(&self, sub: SubId) -> Option<Timestamp> {
        let mut state = self.lock();
        state.sweep(Timestamp::now());
        state.exhausted.get(&sub).copied()
    }

    /// For `SubHealth::Exhausted` in the next snapshot.
    #[must_use]
    pub fn exhaustions(&self) -> Vec<(SubId, Timestamp)> {
        let mut state = self.lock();
        state.sweep(Timestamp::now());
        state.exhausted.iter().map(|(s, u)| (*s, *u)).collect()
    }

    /// Lift `sub`'s quarantine early.
    pub fn clear_exhaustion(&self, sub: SubId) {
        self.lock().exhausted.remove(&sub);
    }

    /// What [`Router::select_in`] would consider right now, in `SubId` order.
    #[must_use]
    pub fn eligible(&self, provider: Provider, subs: &[SubStatus]) -> Vec<SubId> {
        self.eligible_in(provider, subs, None)
    }

    /// [`Router::eligible`], restricted to one pool. `None` is the whole proxy.
    #[must_use]
    pub fn eligible_in(
        &self,
        provider: Provider,
        subs: &[SubStatus],
        pool: Option<&PoolGate>,
    ) -> Vec<SubId> {
        let mut state = self.lock();
        state.sweep(Timestamp::now());
        if !state.settings.proxies(provider) {
            return Vec::new();
        }
        let mut ids: Vec<SubId> = subs
            .iter()
            .filter(|s| Self::usable(s, provider) && !state.exhausted.contains_key(&s.sub))
            .filter(|s| pool.is_none_or(|gate| gate.admits(s)))
            .map(|s| s.sub)
            .collect();
        ids.sort_unstable();
        ids
    }

    fn usable(sub: &SubStatus, provider: Provider) -> bool {
        sub.provider == provider && sub.enabled && !sub.needs_login
    }

    /// With `auto_switch` off a [`Disposition::Rotate`] becomes
    /// [`Disposition::PassThrough`], but the quarantine still happens.
    pub fn on_failure(
        &self,
        sub: SubId,
        class: FailureClass,
        usage: Option<&Usage>,
    ) -> Disposition {
        let mut state = self.lock();
        if class.quarantines() {
            state.exhaust(sub, usage, Timestamp::now());
        }
        match class.disposition() {
            Disposition::Rotate if !state.settings.auto_switch => Disposition::PassThrough,
            other => other,
        }
    }

    /// Pick the sub to serve one request.
    /// `pool` filters before every other axis, so a pinned sub outside it is
    /// passed over rather than dragging the pool open.
    pub async fn select_in(
        &self,
        provider: Provider,
        subs: &[SubStatus],
        hints: Hints,
        pool: Option<&PoolGate>,
        scorer: &dyn UsageScorer,
    ) -> Result<Selection, SelectError> {
        let now = Timestamp::now();

        // Everything up to stickiness: under the lock, with no I/O.
        let (live, settings) = {
            let mut state = self.lock();
            state.sweep(now);
            let settings = state.settings;

            if !settings.proxies(provider) {
                return Err(SelectError::NotProxied { provider });
            }

            let usable: Vec<&SubStatus> =
                subs.iter().filter(|s| Self::usable(s, provider)).collect();
            if usable.is_empty() {
                return Err(SelectError::NoCandidates { provider });
            }

            // Membership and ceiling are reported separately: an empty pool is
            // a config mistake, a pool over its ceiling is working as asked.
            let usable: Vec<&SubStatus> = match pool {
                None => usable,
                Some(gate) => {
                    let members: Vec<&SubStatus> = usable
                        .into_iter()
                        .filter(|s| gate.contains(s.sub))
                        .collect();
                    if members.is_empty() {
                        return Err(SelectError::PoolEmpty {
                            pool: gate.name.clone(),
                            over_ceiling: false,
                        });
                    }
                    let admitted: Vec<&SubStatus> =
                        members.iter().copied().filter(|s| gate.admits(s)).collect();
                    if admitted.is_empty() {
                        return Err(SelectError::PoolEmpty {
                            pool: gate.name.clone(),
                            over_ceiling: gate.has_ceiling(),
                        });
                    }
                    admitted
                }
            };

            let live: Vec<SubStatus> = usable
                .iter()
                .filter(|s| !state.exhausted.contains_key(&s.sub))
                .map(|s| (*s).clone())
                .collect();
            if live.is_empty() {
                let next_reset = usable
                    .iter()
                    .filter_map(|s| state.exhausted.get(&s.sub).copied())
                    .min();
                return Err(SelectError::AllExhausted {
                    provider,
                    next_reset,
                });
            }

            let holds = |sub: SubId| live.iter().any(|s| s.sub == sub);
            let count = live.len();
            let slot = provider.index();

            if let Some(pinned) = state.pinned.filter(|p| holds(*p)) {
                state.current[slot] = Some(pinned);
                return Ok(Selection {
                    sub: pinned,
                    reason: SelectReason::Pinned,
                    candidates: count,
                });
            }

            // A frozen sub that became unusable falls through and re-freezes.
            if !settings.auto_switch
                && let Some(current) = state.current[slot].filter(|c| holds(*c))
            {
                return Ok(Selection {
                    sub: current,
                    reason: SelectReason::Frozen,
                    candidates: count,
                });
            }

            // Ahead of the key: hopping here breaks the conversation upstream.
            if let Some(hinted) = hints.chain.filter(|a| holds(*a)) {
                state.current[slot] = Some(hinted);
                return Ok(Selection {
                    sub: hinted,
                    reason: SelectReason::Affinity,
                    candidates: count,
                });
            }

            if let Some(placed) = hints.key.filter(|k| holds(*k)) {
                state.current[slot] = Some(placed);
                return Ok(Selection {
                    sub: placed,
                    reason: SelectReason::Placement,
                    candidates: count,
                });
            }

            // Before any usage fetch: a ranking applies at rotation time.
            if settings.sticky()
                && let Some(current) = state.current[slot].filter(|c| holds(*c))
            {
                return Ok(Selection {
                    sub: current,
                    reason: SelectReason::Sticky,
                    candidates: count,
                });
            }

            (live, settings)
        };

        // One round, one shared deadline, lock NOT held.
        let strategy = strategy::for_kind(settings.strategy);
        let ids: Vec<SubId> = live.iter().map(|s| s.sub).collect();
        let scores = if strategy.needs_usage() {
            scorer.usage_round(&ids, settings.usage_deadline).await
        } else {
            Vec::new()
        };

        let mut state = self.lock();
        let slot = provider.index();
        let now = Timestamp::now();

        if strategy.needs_usage() {
            for (index, status) in live.iter().enumerate() {
                // A failed fetch never quarantines: our network, not their quota.
                let Some(Some(usage)) = scores.get(index) else {
                    continue;
                };
                if is_exhausted(usage) {
                    state.exhaust(status.sub, Some(usage), now);
                }
            }
        }

        let candidates: Vec<Candidate> = live
            .iter()
            .enumerate()
            .filter(|(_, s)| !state.exhausted.contains_key(&s.sub))
            .map(|(index, s)| {
                let pct = scores
                    .get(index)
                    .and_then(|u| u.as_ref())
                    .map(effective_pct);
                s.candidate(pct)
            })
            .collect();

        if candidates.is_empty() {
            let next_reset = live
                .iter()
                .filter_map(|s| state.exhausted.get(&s.sub).copied())
                .min();
            return Err(SelectError::AllExhausted {
                provider,
                next_reset,
            });
        }

        let index = strategy.pick(&candidates, &mut state.strategies[slot]);
        let sub = candidates[index].sub;
        state.current[slot] = Some(sub);
        Ok(Selection {
            sub,
            reason: SelectReason::Strategy(settings.strategy),
            candidates: candidates.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::model::UsageWindow;

    use super::*;

    const CODEX: Provider = Provider::Codex;

    /// Subs absent from the map read as a failed fetch.
    #[derive(Debug, Clone, Default)]
    struct FixedUsage {
        usage: BTreeMap<SubId, Usage>,
    }

    impl FixedUsage {
        fn new(entries: impl IntoIterator<Item = (SubId, Usage)>) -> Self {
            Self {
                usage: entries.into_iter().collect(),
            }
        }

        fn from_pcts(entries: impl IntoIterator<Item = (SubId, f32)>) -> Self {
            Self::new(entries.into_iter().map(|(sub, pct)| {
                (
                    sub,
                    Usage {
                        session: Some(UsageWindow::from_pct(pct)),
                        ..Usage::default()
                    },
                )
            }))
        }
    }

    impl UsageScorer for FixedUsage {
        fn usage_round<'a>(&'a self, subs: &'a [SubId], _deadline: Duration) -> UsageRound<'a> {
            Box::pin(async move { subs.iter().map(|s| self.usage.get(s).cloned()).collect() })
        }
    }

    /// Counts rounds, so a test can assert the router paid for usage — or not.
    #[derive(Default)]
    struct CountingScorer {
        calls: AtomicUsize,
        inner: FixedUsage,
    }

    impl CountingScorer {
        fn with_pcts(entries: impl IntoIterator<Item = (u32, f32)>) -> Self {
            Self {
                inner: FixedUsage::from_pcts(entries.into_iter().map(|(id, pct)| (SubId(id), pct))),
                ..Self::default()
            }
        }

        fn with_usage(entries: impl IntoIterator<Item = (u32, Usage)>) -> Self {
            Self {
                inner: FixedUsage::new(entries.into_iter().map(|(id, u)| (SubId(id), u))),
                ..Self::default()
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl UsageScorer for CountingScorer {
        fn usage_round<'a>(&'a self, ids: &'a [SubId], deadline: Duration) -> UsageRound<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.usage_round(ids, deadline)
        }
    }

    fn subs(ids: &[u32]) -> Vec<SubStatus> {
        ids.iter()
            .map(|&i| SubStatus::new(SubId(i), CODEX))
            .collect()
    }

    /// Subs with a known account-wide allowance, which is what a ceiling reads.
    fn subs_at(entries: &[(u32, f32, f32)]) -> Vec<SubStatus> {
        entries
            .iter()
            .map(|&(id, session, weekly)| SubStatus {
                session_pct: Some(session),
                weekly_pct: Some(weekly),
                ..SubStatus::new(SubId(id), CODEX)
            })
            .collect()
    }

    fn chained(sub: u32) -> Hints {
        Hints {
            chain: Some(SubId(sub)),
            ..Hints::default()
        }
    }

    fn placed(sub: u32) -> Hints {
        Hints {
            key: Some(SubId(sub)),
            ..Hints::default()
        }
    }

    fn window(pct: f32, resets_in: SignedDuration, now: Timestamp) -> UsageWindow {
        UsageWindow {
            pct,
            resets_at: Some(now + resets_in),
            started_at: None,
        }
    }

    fn usage(session: Option<UsageWindow>, weekly: Option<UsageWindow>) -> Usage {
        Usage {
            session,
            weekly,
            ..Usage::default()
        }
    }

    fn hours(n: i64) -> SignedDuration {
        SignedDuration::from_hours(n)
    }

    fn router(strategy: StrategyKind) -> Router {
        Router::new(RouterSettings {
            strategy,
            ..RouterSettings::default()
        })
    }

    fn gate(name: &str, members: Option<&[u32]>, session: f32, weekly: f32) -> PoolGate {
        PoolGate {
            name: name.to_string(),
            members: members.map(|m| m.iter().map(|&i| SubId(i)).collect()),
            max_session_pct: session,
            max_weekly_pct: weekly,
        }
    }

    /// The isolation guarantee: this pool cannot spend account 3, ever.
    #[tokio::test]
    async fn a_pool_can_only_ever_reach_its_own_members() {
        let router = router(StrategyKind::RoundRobin);
        let all = subs(&[1, 2, 3]);
        let moonshot = gate("moonshot", Some(&[1, 2]), 100.0, 100.0);

        for _ in 0..6 {
            let picked = router
                .select_in(
                    CODEX,
                    &all,
                    Hints::default(),
                    Some(&moonshot),
                    &FixedUsage::default(),
                )
                .await
                .unwrap();
            assert_ne!(picked.sub, SubId(3), "account 3 was never in the pool");
            assert_eq!(picked.candidates, 2);
        }
    }

    #[tokio::test]
    async fn a_ceiling_holds_back_a_member_that_is_too_full() {
        let router = router(StrategyKind::LowestUsage);
        // 1 is over half its week; 2 is not.
        let all = subs_at(&[(1, 10.0, 60.0), (2, 10.0, 20.0)]);
        let moonshot = gate("moonshot", None, 100.0, 50.0);

        let picked = router
            .select_in(
                CODEX,
                &all,
                Hints::default(),
                Some(&moonshot),
                &FixedUsage::default(),
            )
            .await
            .unwrap();
        assert_eq!(picked.sub, SubId(2));
        assert_eq!(
            picked.candidates, 1,
            "the full account never entered the running"
        );
    }

    #[tokio::test]
    async fn a_pool_that_admits_nothing_says_which_kind_of_nothing() {
        let router = router(StrategyKind::LowestUsage);

        // Every member over the ceiling: the pool is working as configured,
        // holding its headroom for others, so a client's own backoff is right.
        let over = router
            .select_in(
                CODEX,
                &subs_at(&[(1, 10.0, 60.0), (2, 10.0, 80.0)]),
                Hints::default(),
                Some(&gate("moonshot", None, 100.0, 50.0)),
                &FixedUsage::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            over,
            SelectError::PoolEmpty {
                pool: "moonshot".into(),
                over_ceiling: true
            }
        );
        assert_eq!(over.status(), 429);
        assert!(
            over.to_string().contains("reserved for other pools"),
            "{over}"
        );

        // A pool naming nobody is a config mistake no retry will fix.
        let typo = router
            .select_in(
                CODEX,
                &subs(&[1, 2]),
                Hints::default(),
                Some(&gate("typo", Some(&[99]), 100.0, 100.0)),
                &FixedUsage::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            typo,
            SelectError::PoolEmpty {
                pool: "typo".into(),
                over_ceiling: false
            }
        );
        assert_eq!(typo.status(), 503, "a config mistake is not a rate limit");
    }

    #[tokio::test]
    async fn a_pin_outside_the_pool_does_not_drag_the_pool_open() {
        let router = router(StrategyKind::LowestUsage);
        let all = subs(&[1, 2]);
        router.pin(Some(SubId(2)));
        let moonshot = gate("moonshot", Some(&[1]), 100.0, 100.0);

        let picked = router
            .select_in(
                CODEX,
                &all,
                Hints::default(),
                Some(&moonshot),
                &FixedUsage::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            picked.sub,
            SubId(1),
            "the pin was passed over, not honoured"
        );
    }

    #[test]
    fn eligible_in_narrows_to_the_pool() {
        let router = router(StrategyKind::LowestUsage);
        let all = subs_at(&[(1, 10.0, 10.0), (2, 10.0, 90.0), (3, 10.0, 10.0)]);
        let moonshot = gate("moonshot", Some(&[1, 2]), 100.0, 50.0);

        assert_eq!(
            router.eligible(CODEX, &all),
            vec![SubId(1), SubId(2), SubId(3)]
        );
        assert_eq!(
            router.eligible_in(CODEX, &all, Some(&moonshot)),
            vec![SubId(1)],
            "2 is over the ceiling, 3 is not a member"
        );
    }

    #[test]
    fn a_ceiling_excludes_at_the_boundary_but_never_an_unpolled_account() {
        let gate = gate("p", None, 50.0, 100.0);
        let at_ceiling = SubStatus {
            session_pct: Some(50.0),
            ..SubStatus::new(SubId(1), CODEX)
        };
        let under = SubStatus {
            session_pct: Some(49.9),
            ..SubStatus::new(SubId(1), CODEX)
        };

        assert!(!gate.admits(&at_ceiling), "a ceiling reached, not passed");
        assert!(gate.admits(&under));
        // Never polled: excluding it would turn a cold start into a dead pool.
        assert!(gate.admits(&SubStatus::new(SubId(1), CODEX)));
    }

    #[test]
    fn usage_limit_matcher_accepts_every_marker_case_insensitively() {
        for marker in USAGE_LIMIT_MARKERS {
            let body = format!(r#"{{"error":{{"message":"{marker}"}}}}"#);
            assert!(is_usage_limit_error(429, &body), "{marker}");
            assert!(is_usage_limit_error(429, &body.to_uppercase()), "{marker}");
            assert!(is_usage_limit_error(429, &body.to_lowercase()), "{marker}");
            assert!(!is_usage_limit_error(500, &body), "{marker}");
        }
    }

    #[test]
    fn failures_classify_and_dispose_as_the_table_says() {
        let limit = r#"{"error":{"message":"Monthly usage limit reached (GoUsageLimitError)"}}"#;
        let cases = [
            (
                Attempt::new(429, limit),
                FailureClass::UsageLimit,
                Disposition::Rotate,
            ),
            // A plain 429 must not rotate: one burst would hit every account.
            (
                Attempt::new(
                    429,
                    r#"{"error":{"type":"rate_limit_error","message":"Number of requests has exceeded your rate limit"}}"#,
                ),
                FailureClass::RateLimited,
                Disposition::PassThrough,
            ),
            (
                Attempt::new(429, ""),
                FailureClass::RateLimited,
                Disposition::PassThrough,
            ),
            (
                Attempt::new(401, "unauthorized"),
                FailureClass::AuthRetryable,
                Disposition::RetrySameSub,
            ),
            (
                Attempt::new(401, "unauthorized").after_auth_retry(),
                FailureClass::AuthPermanent,
                Disposition::Rotate,
            ),
            (
                Attempt::new(400, "bad request").request_scoped(),
                FailureClass::RequestScoped,
                Disposition::PassThrough,
            ),
            (
                Attempt::new(500, "boom"),
                FailureClass::Passthrough,
                Disposition::PassThrough,
            ),
        ];

        for (attempt, class, disposition) in cases {
            assert_eq!(attempt.classify(), class, "{attempt:?}");
            assert_eq!(class.disposition(), disposition, "{attempt:?}");
        }

        assert_eq!(
            classify_refresh(Some(403)),
            FailureClass::RefreshPermanent,
            "400/401/403 from the token endpoint is a dead refresh token"
        );
        for status in [None, Some(500), Some(503), Some(429)] {
            assert_eq!(
                classify_refresh(status),
                FailureClass::RefreshTransient,
                "{status:?}"
            );
        }
        assert_eq!(
            FailureClass::RefreshTransient.disposition(),
            Disposition::Fail { status: 502 }
        );
        assert!(!FailureClass::RefreshTransient.quarantines());
    }

    #[test]
    fn a_request_scoped_429_does_not_rotate_even_though_its_body_reads_as_a_usage_limit() {
        let body =
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"usage limit"}}"#;
        assert_eq!(Attempt::new(429, body).classify(), FailureClass::UsageLimit);

        let scoped = Attempt::new(429, body).request_scoped();
        assert_eq!(scoped.classify(), FailureClass::RequestScoped);
        assert!(!FailureClass::RequestScoped.quarantines());

        let router = Router::default();
        let disposition = router.on_failure(SubId(1), scoped.classify(), None);
        assert_eq!(disposition, Disposition::PassThrough);
        assert_eq!(router.exhausted_until(SubId(1)), None);
    }

    #[test]
    fn auto_switch_off_turns_rotation_into_passthrough_but_still_records_the_quarantine() {
        let router = Router::new(RouterSettings {
            auto_switch: false,
            ..RouterSettings::default()
        });
        let disposition = router.on_failure(SubId(1), FailureClass::UsageLimit, None);
        assert_eq!(disposition, Disposition::PassThrough);
        assert!(router.exhausted_until(SubId(1)).is_some());
    }

    #[test]
    fn exhaust_until_waits_for_the_reset_that_actually_frees_the_account() {
        let now = Timestamp::now();

        // A full window has to roll over, even when another resets sooner.
        let u = usage(
            Some(window(100.0, hours(3), now)),
            Some(window(50.0, hours(1), now)),
        );
        assert_eq!(exhaust_until(now, Some(&u)), now + hours(3));
        let u = usage(
            Some(window(100.0, hours(1), now)),
            Some(window(100.0, hours(50), now)),
        );
        assert_eq!(exhaust_until(now, Some(&u)), now + hours(50));

        // Nothing full: usage lags the refusal, so retry at the nearest reset.
        let u = usage(
            Some(window(40.0, hours(1), now)),
            Some(window(10.0, hours(100), now)),
        );
        assert_eq!(exhaust_until(now, Some(&u)), now + hours(1));

        // No usable window: none at all, none with a reset time, none ahead.
        assert_eq!(exhaust_until(now, None), now + BLIND_EXHAUSTION);
        assert_eq!(
            exhaust_until(now, Some(&Usage::default())),
            now + BLIND_EXHAUSTION
        );
        let u = usage(Some(UsageWindow::from_pct(100.0)), None);
        assert_eq!(exhaust_until(now, Some(&u)), now + BLIND_EXHAUSTION);
        let u = usage(Some(window(100.0, hours(-1), now)), None);
        assert_eq!(exhaust_until(now, Some(&u)), now + BLIND_EXHAUSTION);
    }

    #[tokio::test]
    async fn exhausting_the_current_sub_clears_stickiness() {
        let router = router(StrategyKind::LowestUsage);
        let all = subs(&[1, 2]);
        let scorer = CountingScorer::with_pcts([(1, 10.0), (2, 40.0)]);

        let first = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap();
        assert_eq!(first.sub, SubId(1));
        assert_eq!(router.current(CODEX), Some(SubId(1)));

        router.exhaust(SubId(1), None);
        assert_eq!(router.current(CODEX), None, "current must be cleared");

        let second = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap();
        assert_eq!(second.sub, SubId(2));
        assert_eq!(second.candidates, 1, "the exhausted sub is not a candidate");
        assert_eq!(router.exhaustions().len(), 1);
        router.clear_exhaustion(SubId(1));
        assert_eq!(router.exhausted_until(SubId(1)), None);
    }

    #[tokio::test]
    async fn an_expired_quarantine_is_swept_before_selection() {
        let router = router(StrategyKind::LowestUsage);
        let all = subs(&[1]);
        let scorer = CountingScorer::with_pcts([(1, 10.0)]);

        router.exhaust(SubId(1), None);
        assert!(
            router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .is_err()
        );

        router
            .lock()
            .exhausted
            .insert(SubId(1), Timestamp::now() - hours(1));
        assert_eq!(router.exhaustions(), Vec::new());
        assert_eq!(
            router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap()
                .sub,
            SubId(1)
        );
    }

    #[tokio::test]
    async fn a_pin_beats_affinity_stickiness_and_the_strategy() {
        let router = router(StrategyKind::LowestUsage);
        let all = subs(&[1, 2, 3]);
        // Sub 3 is the least used, so the strategy would pick it.
        let scorer = CountingScorer::with_pcts([(1, 90.0), (2, 50.0), (3, 1.0)]);

        // Establish 2 as current, so stickiness has something to hold.
        router.pin(Some(SubId(2)));
        assert_eq!(
            router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap()
                .sub,
            SubId(2)
        );

        router.pin(Some(SubId(1)));
        let picked = router
            .select_in(CODEX, &all, chained(3), None, &scorer)
            .await
            .unwrap();
        assert_eq!(picked.sub, SubId(1));
        assert_eq!(picked.reason, SelectReason::Pinned);
        assert_eq!(scorer.calls(), 0, "a pin never pays for usage");

        // A pin naming an unusable sub is ignored rather than fatal.
        router.pin(Some(SubId(99)));
        let picked = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap();
        assert_eq!(picked.sub, SubId(1), "falls through to stickiness");
        assert_eq!(picked.reason, SelectReason::Sticky);

        router.clear_current(CODEX);
        let picked = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap();
        assert_eq!(picked.sub, SubId(3), "and then to the strategy");
        assert_eq!(
            picked.reason,
            SelectReason::Strategy(StrategyKind::LowestUsage)
        );

        router.pin(None);
        assert_eq!(router.pinned(), None);
    }

    /// "Lowest usage" applies at rotation time, not per request.
    #[tokio::test]
    async fn stickiness_short_circuits_before_the_usage_scorer_is_called() {
        let router = router(StrategyKind::LowestUsage);
        assert!(
            router.settings().sticky(),
            "lowest-usage defaults to sticky"
        );
        let all = subs(&[1, 2]);
        let scorer = CountingScorer::with_pcts([(1, 10.0), (2, 40.0)]);

        let first = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap();
        assert_eq!(first.sub, SubId(1));
        assert_eq!(
            first.reason,
            SelectReason::Strategy(StrategyKind::LowestUsage)
        );
        assert_eq!(scorer.calls(), 1, "the first select ranks, so it fetches");

        for _ in 0..5 {
            let again = router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap();
            assert_eq!(again.sub, SubId(1));
            assert_eq!(again.reason, SelectReason::Sticky);
        }
        assert_eq!(
            scorer.calls(),
            1,
            "the sticky path must not fetch usage at all"
        );
    }

    #[tokio::test]
    async fn sticky_is_an_override_not_a_fifth_strategy() {
        // Sticky round-robin is odd but expressible, and must actually stick.
        let router = Router::new(RouterSettings {
            strategy: StrategyKind::RoundRobin,
            sticky: Some(true),
            ..RouterSettings::default()
        });
        let all = subs(&[1, 2, 3]);
        let scorer = CountingScorer::default();

        let first = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap();
        assert_eq!(first.sub, SubId(1));
        for _ in 0..3 {
            let again = router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap();
            assert_eq!(again.sub, SubId(1));
            assert_eq!(again.reason, SelectReason::Sticky);
        }

        // And the inverse: a ranking strategy told not to stick.
        let router = Router::new(RouterSettings {
            strategy: StrategyKind::LowestUsage,
            sticky: Some(false),
            ..RouterSettings::default()
        });
        let scorer = CountingScorer::with_pcts([(1, 10.0), (2, 40.0), (3, 1.0)]);
        for _ in 0..3 {
            let picked = router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap();
            assert_eq!(picked.sub, SubId(3));
        }
        assert_eq!(scorer.calls(), 3, "not sticky means it ranks every time");
    }

    #[tokio::test]
    async fn a_hint_is_honoured_when_its_sub_is_a_candidate() {
        for (hints, reason) in [
            (chained(1), SelectReason::Affinity),
            (placed(1), SelectReason::Placement),
        ] {
            let router = router(StrategyKind::LowestUsage);
            let all = subs(&[1, 2, 3]);
            // 3 is the least used, so the strategy would pick it instead.
            let scorer = CountingScorer::with_pcts([(1, 90.0), (2, 50.0), (3, 1.0)]);

            let picked = router
                .select_in(CODEX, &all, hints, None, &scorer)
                .await
                .unwrap();
            assert_eq!(picked.sub, SubId(1), "{reason:?}");
            assert_eq!(picked.reason, reason);
            assert_eq!(scorer.calls(), 0, "a hint short-circuits the usage round");
            assert_eq!(router.current(CODEX), Some(SubId(1)));
        }
    }

    /// Hopping a chain breaks the conversation; hopping a key costs only cache.
    #[tokio::test]
    async fn a_chain_hint_outranks_a_key_placement() {
        let router = router(StrategyKind::LowestUsage);
        let all = subs(&[1, 2, 3]);
        let scorer = CountingScorer::with_pcts([(1, 90.0), (2, 50.0), (3, 1.0)]);

        let picked = router
            .select_in(
                CODEX,
                &all,
                Hints {
                    chain: Some(SubId(1)),
                    key: Some(SubId(2)),
                },
                None,
                &scorer,
            )
            .await
            .unwrap();
        assert_eq!(picked.sub, SubId(1));
        assert_eq!(picked.reason, SelectReason::Affinity);
    }

    #[tokio::test]
    async fn an_unusable_chain_falls_to_the_key_and_an_unusable_key_falls_further() {
        let router = Router::new(RouterSettings {
            strategy: StrategyKind::LowestUsage,
            sticky: Some(true),
            ..RouterSettings::default()
        });
        let all = subs(&[1, 2, 3]);
        let scorer = CountingScorer::with_pcts([(1, 90.0), (2, 50.0), (3, 1.0)]);

        router.exhaust(SubId(1), None);
        let picked = router
            .select_in(
                CODEX,
                &all,
                Hints {
                    chain: Some(SubId(1)),
                    key: Some(SubId(2)),
                },
                None,
                &scorer,
            )
            .await
            .unwrap();
        assert_eq!(
            picked.sub,
            SubId(2),
            "the key caught what the chain dropped"
        );
        assert_eq!(picked.reason, SelectReason::Placement);

        // 2 is now `current`, so a hint naming a stranger falls to stickiness ...
        let picked = router
            .select_in(CODEX, &all, placed(77), None, &scorer)
            .await
            .unwrap();
        assert_eq!(picked.sub, SubId(2));
        assert_eq!(picked.reason, SelectReason::Sticky);
        assert_eq!(scorer.calls(), 0);

        // ... and with nothing to stick to either, to the ranking.
        router.clear_current(CODEX);
        let picked = router
            .select_in(CODEX, &all, placed(77), None, &scorer)
            .await
            .unwrap();
        assert_eq!(picked.sub, SubId(3));
        assert_eq!(
            picked.reason,
            SelectReason::Strategy(StrategyKind::LowestUsage)
        );
    }

    #[tokio::test]
    async fn auto_switch_off_never_changes_sub_across_successive_selects() {
        let router = Router::new(RouterSettings {
            strategy: StrategyKind::RoundRobin,
            auto_switch: false,
            ..RouterSettings::default()
        });
        let all = subs(&[1, 2, 3]);
        let scorer = CountingScorer::default();

        let first = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap();
        assert_eq!(first.sub, SubId(1));

        for _ in 0..5 {
            let again = router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap();
            assert_eq!(again.sub, SubId(1), "frozen means frozen");
            assert_eq!(again.reason, SelectReason::Frozen);
        }

        // Not even an affinity hint moves it: identity never changes silently.
        let again = router
            .select_in(CODEX, &all, chained(3), None, &scorer)
            .await
            .unwrap();
        assert_eq!(again.sub, SubId(1));
        assert_eq!(again.reason, SelectReason::Frozen);

        // A pin is the user asking explicitly, so it still wins.
        router.pin(Some(SubId(2)));
        let pinned = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap();
        assert_eq!(pinned.sub, SubId(2));
        assert_eq!(pinned.reason, SelectReason::Pinned);
    }

    #[tokio::test]
    async fn auto_switch_off_re_freezes_when_the_frozen_sub_becomes_unusable() {
        let router = Router::new(RouterSettings {
            auto_switch: false,
            ..RouterSettings::default()
        });
        let mut all = subs(&[1, 2]);
        let scorer = CountingScorer::with_pcts([(1, 10.0), (2, 40.0)]);

        assert_eq!(
            router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap()
                .sub,
            SubId(1)
        );
        // Staying frozen would mean a proxy that is dead until it restarts.
        all[0].enabled = false;
        let picked = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap();
        assert_eq!(picked.sub, SubId(2));
        let again = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap();
        assert_eq!(again.reason, SelectReason::Frozen);
    }

    #[tokio::test]
    async fn the_usage_round_quarantines_a_full_or_cut_off_account() {
        let now = Timestamp::now();
        // The verdict at 40% stands in for a usage endpoint that lags the cutoff.
        let cut_off = Usage {
            limit_reached: Some(true),
            ..usage(Some(window(40.0, hours(2), now)), None)
        };

        for bad in [usage(Some(window(100.0, hours(2), now)), None), cut_off] {
            let router = router(StrategyKind::LowestUsage);
            let all = subs(&[1, 2]);
            let scorer = CountingScorer::with_usage([
                (1, bad),
                (2, usage(Some(window(80.0, hours(2), now)), None)),
            ]);

            let picked = router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap();
            assert_eq!(
                picked.sub,
                SubId(2),
                "the spent account must not win on being the least used"
            );
            assert_eq!(picked.candidates, 1);
            assert_eq!(router.exhausted_until(SubId(1)), Some(now + hours(2)));
            assert_eq!(router.exhausted_until(SubId(2)), None);
        }
    }

    #[test]
    fn is_exhausted_reads_the_verdict_first_and_the_percentage_second() {
        let full = usage(Some(UsageWindow::from_pct(100.0)), None);
        let low = usage(Some(UsageWindow::from_pct(40.0)), None);

        assert!(is_exhausted(&full));
        assert!(!is_exhausted(&low));
        assert!(is_exhausted(&Usage {
            limit_reached: Some(true),
            ..low.clone()
        }));

        // A floor, not a veto: a snapshot already at 100% is still exhausted.
        assert!(!is_exhausted(&Usage {
            limit_reached: Some(false),
            ..low
        }));
        assert!(is_exhausted(&Usage {
            limit_reached: Some(false),
            ..full
        }));
    }

    #[tokio::test]
    async fn a_failed_usage_fetch_never_quarantines_and_is_used_as_a_last_resort() {
        let now = Timestamp::now();
        let router = router(StrategyKind::LowestUsage);
        let all = subs(&[5, 4]); // 5 is confirmed full, 4's usage fetch failed
        let scorer =
            CountingScorer::with_usage([(5, usage(Some(window(100.0, hours(2), now)), None))]);

        let picked = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap();
        assert_eq!(picked.sub, SubId(4));
        assert_eq!(
            router.exhausted_until(SubId(4)),
            None,
            "a failed fetch is not a full account"
        );
        assert!(router.exhausted_until(SubId(5)).is_some());
    }

    #[tokio::test]
    async fn every_candidate_confirmed_full_is_all_exhausted_with_the_soonest_reset() {
        let now = Timestamp::now();
        let router = router(StrategyKind::LowestUsage);
        let all = subs(&[1, 2]);
        let scorer = CountingScorer::with_usage([
            (1, usage(Some(window(100.0, hours(5), now)), None)),
            (2, usage(Some(window(100.0, hours(2), now)), None)),
        ]);

        let err = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap_err();
        let SelectError::AllExhausted { next_reset, .. } = err else {
            panic!("expected AllExhausted, got {err:?}");
        };
        assert_eq!(next_reset, Some(now + hours(2)));
        assert_eq!(err.status(), 429);

        // The second attempt never reaches the usage round and says the same.
        let err = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap_err();
        assert!(matches!(err, SelectError::AllExhausted { .. }));
    }

    #[tokio::test]
    async fn candidates_exclude_the_wrong_provider_the_disabled_and_the_logged_out() {
        let router = router(StrategyKind::RoundRobin);
        let scorer = CountingScorer::default();
        let all = vec![
            SubStatus::new(SubId(1), Provider::Claude),
            SubStatus {
                enabled: false,
                ..SubStatus::new(SubId(2), CODEX)
            },
            SubStatus {
                needs_login: true,
                ..SubStatus::new(SubId(3), CODEX)
            },
            SubStatus::new(SubId(4), CODEX),
        ];

        assert_eq!(router.eligible(CODEX, &all), vec![SubId(4)]);
        let picked = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap();
        assert_eq!(picked.sub, SubId(4));
        assert_eq!(picked.candidates, 1);

        // Even a pin cannot resurrect a sub that needs a login.
        router.pin(Some(SubId(3)));
        assert_eq!(
            router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap()
                .sub,
            SubId(4)
        );
    }

    #[tokio::test]
    async fn a_provider_with_nothing_to_route_says_which_problem_it_is() {
        let scorer = CountingScorer::default();
        let all = subs(&[1]);

        let off = Router::new(RouterSettings {
            providers_proxied: [false, true],
            ..RouterSettings::default()
        });
        let err = off
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap_err();
        assert_eq!(err, SelectError::NotProxied { provider: CODEX });
        assert_eq!(err.status(), 503);
        assert_eq!(off.eligible(CODEX, &all), Vec::new());

        let empty = router(StrategyKind::RoundRobin);
        let err = empty
            .select_in(CODEX, &[], Hints::default(), None, &scorer)
            .await
            .unwrap_err();
        assert_eq!(err, SelectError::NoCandidates { provider: CODEX });
        assert_eq!(err.status(), 503);
    }

    #[tokio::test]
    async fn the_two_providers_route_independently() {
        let router = router(StrategyKind::RoundRobin);
        let scorer = CountingScorer::default();
        let all = vec![
            SubStatus::new(SubId(1), CODEX),
            SubStatus::new(SubId(2), CODEX),
            SubStatus::new(SubId(3), Provider::Claude),
        ];

        assert_eq!(
            router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap()
                .sub,
            SubId(1)
        );
        assert_eq!(
            router
                .select_in(Provider::Claude, &all, Hints::default(), None, &scorer)
                .await
                .unwrap()
                .sub,
            SubId(3)
        );
        // Claude's turn did not advance Codex's cursor.
        assert_eq!(
            router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap()
                .sub,
            SubId(2)
        );
        assert_eq!(router.current(CODEX), Some(SubId(2)));
        assert_eq!(router.current(Provider::Claude), Some(SubId(3)));
    }

    #[tokio::test]
    async fn fails_over_when_the_sticky_sub_is_used_up() {
        let now = Timestamp::now();
        let router = router(StrategyKind::LowestUsage);
        let all = subs(&[1, 2]); // A (full), B
        let scorer = CountingScorer::with_usage([
            (1, usage(Some(window(100.0, hours(1), now)), None)),
            (2, usage(Some(window(40.0, hours(1), now)), None)),
        ]);

        assert_eq!(
            router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap()
                .sub,
            SubId(2)
        );

        let body = r#"{"error":{"message":"Monthly usage limit reached"}}"#;
        let class = Attempt::new(429, body).classify();
        assert_eq!(class, FailureClass::UsageLimit);
        assert_eq!(
            router.on_failure(SubId(2), class, None),
            Disposition::Rotate
        );

        let err = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap_err();
        assert!(matches!(err, SelectError::AllExhausted { .. }));
        assert_eq!(err.status(), 429);
        assert!(err.to_string().contains("used up"));
    }

    #[tokio::test]
    async fn fails_over_after_a_persistent_authorization_error() {
        let router = router(StrategyKind::LowestUsage);
        let all = subs(&[6, 7]);
        let scorer = CountingScorer::with_pcts([(6, 0.0), (7, 10.0)]);

        assert_eq!(
            router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap()
                .sub,
            SubId(6)
        );

        let first = Attempt::new(401, "unauthorized");
        assert_eq!(
            router.on_failure(SubId(6), first.classify(), None),
            Disposition::RetrySameSub
        );
        assert_eq!(router.exhausted_until(SubId(6)), None, "one retry first");

        let second = first.after_auth_retry();
        assert_eq!(
            router.on_failure(SubId(6), second.classify(), None),
            Disposition::Rotate
        );
        assert!(router.exhausted_until(SubId(6)).is_some());

        assert_eq!(
            router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap()
                .sub,
            SubId(7)
        );
    }

    #[tokio::test]
    async fn does_not_quarantine_after_a_transient_refresh_failure() {
        let router = router(StrategyKind::LowestUsage);
        let all = subs(&[11]);
        let scorer = CountingScorer::with_pcts([(11, 5.0)]);

        assert_eq!(
            router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap()
                .sub,
            SubId(11)
        );

        let class = classify_refresh(Some(503));
        assert_eq!(
            router.on_failure(SubId(11), class, None),
            Disposition::Fail { status: 502 }
        );
        assert_eq!(router.exhausted_until(SubId(11)), None);

        let recovered = router
            .select_in(CODEX, &all, Hints::default(), None, &scorer)
            .await
            .unwrap();
        assert_eq!(recovered.sub, SubId(11));
        assert_eq!(recovered.reason, SelectReason::Sticky);
    }

    #[tokio::test]
    async fn changing_strategy_resets_the_round_robin_cursor_but_keeps_the_current_sub() {
        let router = router(StrategyKind::RoundRobin);
        let all = subs(&[1, 2, 3]);
        let scorer = CountingScorer::default();

        assert_eq!(
            router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap()
                .sub,
            SubId(1)
        );
        router.set_settings(RouterSettings {
            strategy: StrategyKind::LeastConnections,
            ..RouterSettings::default()
        });
        assert_eq!(router.current(CODEX), Some(SubId(1)));

        router.set_settings(RouterSettings {
            strategy: StrategyKind::RoundRobin,
            ..RouterSettings::default()
        });
        assert_eq!(
            router
                .select_in(CODEX, &all, Hints::default(), None, &scorer)
                .await
                .unwrap()
                .sub,
            SubId(1),
            "the cursor was reset with the strategy"
        );
    }

    #[test]
    fn effective_pct_takes_the_worse_of_the_two_headline_windows() {
        let now = Timestamp::now();
        let u = usage(
            Some(window(12.0, hours(1), now)),
            Some(window(64.0, hours(1), now)),
        );
        assert!((effective_pct(&u) - 64.0).abs() < f32::EPSILON);
        assert!((effective_pct(&Usage::default())).abs() < f32::EPSILON);
    }
}
