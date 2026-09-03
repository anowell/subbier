//! The usage poller: a 60s cache, a per-sub backoff ladder, one round deadline.
//! A flat error TTL below the success TTL would poll a rate-limited account more
//! often than a healthy one, so failures walk [`UsageCache::BACKOFF`]. A fresh
//! quota signal *replaces* the cached snapshot; a 401 is reported, not refreshed.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;
use tracing::Instrument;

use crate::error::{Error, Result};
use crate::model::{Provider, Sub, SubId, SubKey, Usage};
use crate::provider;

pub use crate::provider::{is_deadline_exceeded, is_unauthorized};

/// The upstream API base per provider, resolved once so a test can point one at a fake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bases {
    pub codex: String,
    pub claude: String,
}

impl Bases {
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            codex: Provider::Codex.upstream_base(),
            claude: Provider::Claude.upstream_base(),
        }
    }

    /// Both providers pointed at one base. For fake upstreams in tests.
    #[must_use]
    pub fn all(base: impl Into<String>) -> Self {
        let base = base.into();
        Self {
            codex: base.clone(),
            claude: base,
        }
    }

    #[must_use]
    pub fn get(&self, provider: Provider) -> &str {
        match provider {
            Provider::Codex => &self.codex,
            Provider::Claude => &self.claude,
        }
    }
}

impl Default for Bases {
    fn default() -> Self {
        Self::from_env()
    }
}

#[derive(Debug)]
pub struct CacheEntry {
    /// For a failure, how much of its backoff is left; `None` for a success.
    pub retry_in: Option<Duration>,
    /// `Err` means the *fetch* failed, not that the account is exhausted.
    pub usage: std::result::Result<Usage, Error>,
}

/// Per-sub usage with a TTL, keyed by the persisted [`SubKey`].
#[derive(Debug, Default)]
pub struct UsageCache {
    entries: Mutex<HashMap<SubKey, Entry>>,
    bases: Bases,
}

#[derive(Debug, Clone)]
struct Entry {
    fetched_at: Instant,
    result: std::result::Result<Usage, CachedError>,
    /// Consecutive failed polls; `0` for a success, which resets the ladder.
    failures: u32,
    ttl: Duration,
}

impl Entry {
    fn ok(usage: Usage) -> Self {
        Self {
            fetched_at: Instant::now(),
            result: Ok(usage),
            failures: 0,
            ttl: UsageCache::TTL,
        }
    }

    /// A failed sample, one rung further down the ladder than `previous`.
    fn failed(error: CachedError, previous: u32) -> Self {
        let failures = previous.saturating_add(1);
        Self {
            ttl: UsageCache::backoff(failures, &error),
            fetched_at: Instant::now(),
            result: Err(error),
            failures,
        }
    }
}

impl UsageCache {
    /// How long a successful poll is reused.
    pub const TTL: Duration = Duration::from_secs(60);

    /// Indexed by `failures - 1`, saturating at the last rung. A 429 starts a
    /// rung higher, so being rate limited never buys a faster poll than health.
    pub const BACKOFF: [Duration; 5] = [
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(120),
        Duration::from_secs(300),
        Duration::from_secs(900),
    ];

    /// A 429 starts one rung higher than a generic failure, and `Retry-After`
    /// is a floor, not a target: honoured only above the computed backoff.
    #[must_use]
    fn backoff(failures: u32, error: &CachedError) -> Duration {
        let rung = (failures.saturating_sub(1) as usize)
            .saturating_add(usize::from(error.is_rate_limited()));
        let computed = Self::BACKOFF[rung.min(Self::BACKOFF.len() - 1)];
        error
            .retry_after()
            .filter(|floor| *floor > computed)
            .unwrap_or(computed)
    }

    #[must_use]
    pub fn new() -> Self {
        Self::with_bases(Bases::from_env())
    }

    /// A cache aimed at explicit upstream bases. For the proxy test harness.
    #[must_use]
    pub fn with_bases(bases: Bases) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            bases,
        }
    }

    /// This sub's usage, from cache when fresh.
    ///
    /// An `Err` is a failed fetch — 401 included — never an exhausted account.
    pub async fn get(&self, sub: &Sub, force: bool, deadline: Instant) -> Result<Usage> {
        let span = tracing::info_span!(
            "usage.poll",
            sub = %sub.key,
            provider = sub.provider.id(),
            cached = tracing::field::Empty,
            session_pct = tracing::field::Empty,
            weekly_pct = tracing::field::Empty,
        );
        async move {
            if !force && let Some(hit) = self.fresh(&sub.key) {
                record(&tracing::Span::current(), true, &hit);
                return hit;
            }
            let result = provider::fetch_usage_at(
                sub.provider,
                self.bases.get(sub.provider),
                &sub.credentials,
                deadline,
            )
            .await;
            self.store(sub.key.clone(), &result);
            record(&tracing::Span::current(), false, &result);
            result
        }
        .instrument(span)
        .await
    }

    /// Score several subs concurrently against **one shared deadline**, so a
    /// hung account never blocks a healthy account's score.
    pub async fn score_all(
        &self,
        subs: &[(SubId, &Sub)],
        deadline: Instant,
    ) -> Vec<(SubId, Result<Usage>)> {
        self.poll_all(subs, false, deadline).await
    }

    /// [`UsageCache::score_all`], bypassing the cache. It does not invalidate
    /// first: that would reset every backoff ladder too.
    pub async fn refresh_all(
        &self,
        subs: &[(SubId, &Sub)],
        deadline: Instant,
    ) -> Vec<(SubId, Result<Usage>)> {
        self.poll_all(subs, true, deadline).await
    }

    async fn poll_all(
        &self,
        subs: &[(SubId, &Sub)],
        force: bool,
        deadline: Instant,
    ) -> Vec<(SubId, Result<Usage>)> {
        let polls = subs
            .iter()
            .map(|(id, sub)| async move { (*id, self.get(sub, force, deadline).await) });
        futures_util::future::join_all(polls).await
    }

    /// Record a quota signal seen off a proxied response. **Replaces** the
    /// snapshot rather than merging, and clears the backoff like a poll.
    pub fn observe(&self, key: &SubKey, usage: Usage) {
        self.lock().insert(key.clone(), Entry::ok(usage));
    }

    /// What is cached for this sub, fresh or not, plus the error that stalled it.
    #[must_use]
    pub fn peek(&self, key: &SubKey) -> Option<CacheEntry> {
        let entry = self.lock().get(key)?.clone();
        let age = Instant::now().saturating_duration_since(entry.fetched_at);
        Some(CacheEntry {
            retry_in: entry.result.is_err().then(|| entry.ttl.saturating_sub(age)),
            usage: entry.result.map_err(|e| e.into_error()),
        })
    }

    /// Drop one sub's entry, so the next read re-polls.
    pub fn invalidate(&self, key: &SubKey) {
        self.lock().remove(key);
    }

    fn fresh(&self, key: &SubKey) -> Option<Result<Usage>> {
        let entry = self.lock().get(key)?.clone();
        let age = Instant::now().saturating_duration_since(entry.fetched_at);
        (age < entry.ttl).then(|| entry.result.map_err(|e| e.into_error()))
    }

    fn store(&self, key: SubKey, result: &Result<Usage>) {
        let mut entries = self.lock();
        let entry = match result {
            Ok(usage) => Entry::ok(usage.clone()),
            Err(e) => {
                let previous = entries.get(&key).map_or(0, |e| e.failures);
                Entry::failed(CachedError::capture(e), previous)
            }
        };
        entries.insert(key, entry);
    }

    /// A panic cannot corrupt the map: every critical section here is a single
    /// insert, remove or clone.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SubKey, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// [`Error`] is not `Clone`, so a cached failure keeps only the parts
/// [`is_unauthorized`] and [`is_deadline_exceeded`] classify, rebuilt on the way out.
#[derive(Debug, Clone)]
enum CachedError {
    Upstream {
        status: u16,
        message: String,
        retry_after: Option<Duration>,
    },
    Deadline,
    Other(String),
}

impl CachedError {
    fn capture(e: &Error) -> Self {
        match e {
            Error::Upstream {
                status,
                message,
                retry_after,
            } => CachedError::Upstream {
                status: *status,
                message: message.clone(),
                retry_after: *retry_after,
            },
            _ if is_deadline_exceeded(e) => CachedError::Deadline,
            other => CachedError::Other(other.to_string()),
        }
    }

    fn into_error(self) -> Error {
        match self {
            CachedError::Upstream {
                status,
                message,
                retry_after,
            } => Error::upstream_after(status, message, retry_after),
            CachedError::Deadline => provider::deadline_exceeded(),
            CachedError::Other(message) => Error::other(message),
        }
    }

    const fn is_rate_limited(&self) -> bool {
        matches!(self, CachedError::Upstream { status: 429, .. })
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            CachedError::Upstream { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// Fill in the `usage.poll` span. Percentages only; never a token.
fn record(span: &tracing::Span, cached: bool, result: &Result<Usage>) {
    span.record("cached", cached);
    if let Ok(usage) = result {
        if let Some(session) = usage.session {
            span.record("session_pct", f64::from(session.pct));
        }
        if let Some(weekly) = usage.weekly {
            span.record("weekly_pct", f64::from(weekly.pct));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::get;

    use super::*;
    use crate::model::{CredentialSource, Credentials, Tokens};

    const CODEX_BODY: &str = include_str!("../provider/fixtures/codex_usage.json");
    const CLAUDE_BODY: &str = include_str!("../provider/fixtures/claude_usage.json");

    fn sub(provider: Provider, account: &str) -> Sub {
        Sub {
            key: SubKey::new(provider, account),
            provider,
            label: account.to_owned(),
            credentials: Credentials {
                plan: None,
                account_id: Some(account.to_owned()),
                email: None,
                tokens: Tokens {
                    access: "test-token".into(),
                    refresh: None,
                    expires_at: None,
                },
                source: CredentialSource::Subbier,
            },
        }
    }

    /// `/wham/usage` hangs forever; returns the base URL and the Claude hit counter.
    async fn fake_upstream(status: StatusCode) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/wham/usage",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    CODEX_BODY
                }),
            )
            .route(
                "/api/oauth/usage",
                get(
                    move |State((hits, status)): State<(Arc<AtomicUsize>, StatusCode)>| async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        (status, CLAUDE_BODY)
                    },
                ),
            )
            .with_state((hits.clone(), status));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, hits)
    }

    #[tokio::test]
    async fn score_all_returns_the_healthy_sub_within_the_shared_deadline() {
        let (base, _) = fake_upstream(StatusCode::OK).await;
        let cache = UsageCache::with_bases(Bases::all(base));
        let hung = sub(Provider::Codex, "hangs");
        let healthy = sub(Provider::Claude, "healthy");

        let started = Instant::now();
        let deadline = started + Duration::from_millis(400);
        let scores = cache
            .score_all(&[(SubId(1), &hung), (SubId(2), &healthy)], deadline)
            .await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(2000),
            "one hung account blocked the round: {elapsed:?}"
        );
        assert_eq!(scores.len(), 2);
        assert_eq!(scores[0].0, SubId(1));
        assert_eq!(scores[1].0, SubId(2));

        let hung_result = scores[0].1.as_ref().unwrap_err();
        assert!(
            is_deadline_exceeded(hung_result),
            "expected a deadline error, got {hung_result}"
        );
        assert!(!is_unauthorized(hung_result), "a hung fetch is not a 401");

        let healthy_usage = scores[1].1.as_ref().expect("the healthy sub scored");
        assert_eq!(healthy_usage.weekly.unwrap().pct, 42.0);
    }

    #[tokio::test]
    async fn a_fresh_entry_is_reused_and_force_bypasses_it() {
        let (base, hits) = fake_upstream(StatusCode::OK).await;
        let cache = UsageCache::with_bases(Bases::all(base));
        let s = sub(Provider::Claude, "cached");
        let deadline = || Instant::now() + Duration::from_secs(5);

        cache.get(&s, false, deadline()).await.unwrap();
        cache.get(&s, false, deadline()).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "second read was not cached");

        cache.get(&s, true, deadline()).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2, "force must bypass the TTL");

        assert!(cache.peek(&s.key).expect("an entry").usage.is_ok());

        cache.invalidate(&s.key);
        assert!(cache.peek(&s.key).is_none());
        cache.get(&s, false, deadline()).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn an_upstream_401_survives_the_cache_as_a_401() {
        let (base, _) = fake_upstream(StatusCode::UNAUTHORIZED).await;
        let cache = UsageCache::with_bases(Bases::all(base));
        let s = sub(Provider::Claude, "expired");
        let deadline = Instant::now() + Duration::from_secs(5);

        let first = cache.get(&s, false, deadline).await;
        assert!(is_unauthorized(first.as_ref().unwrap_err()));

        // Rebuilt from the cache, still classified the same way.
        let cached = cache.peek(&s.key).expect("an entry");
        assert!(is_unauthorized(cached.usage.as_ref().unwrap_err()));
        assert!(!is_deadline_exceeded(cached.usage.as_ref().unwrap_err()));
    }

    #[tokio::test]
    async fn observe_replaces_the_snapshot_rather_than_merging_into_it() {
        let (base, _) = fake_upstream(StatusCode::OK).await;
        let cache = UsageCache::with_bases(Bases::all(base));
        let s = sub(Provider::Claude, "observed");
        cache
            .get(&s, false, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        let polled = cache.peek(&s.key).unwrap().usage.unwrap();
        assert!(polled.weekly.unwrap().resets_at.is_some());
        assert_eq!(polled.scoped.len(), 1);

        // A watermark that no longer applies must not survive.
        let fresh = Usage {
            session: Some(crate::model::UsageWindow::from_pct(9.0)),
            ..Usage::default()
        };
        cache.observe(&s.key, fresh);

        let after = cache.peek(&s.key).unwrap().usage.unwrap();
        assert_eq!(after.session.unwrap().pct, 9.0);
        assert_eq!(after.weekly, None, "the stale weekly window must be gone");
        assert!(after.scoped.is_empty());
    }

    #[tokio::test]
    async fn a_failed_fetch_is_not_a_hundred_percent() {
        let (base, _) = fake_upstream(StatusCode::INTERNAL_SERVER_ERROR).await;
        let cache = UsageCache::with_bases(Bases::all(base));
        let s = sub(Provider::Claude, "broken");
        let err = cache
            .get(&s, false, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Upstream { status: 500, .. }));
        assert!(!is_unauthorized(&err));
        assert!(!is_deadline_exceeded(&err));
    }

    /// As [`fake_upstream`], but the Claude route answers with a `retry-after`.
    async fn throttling_upstream(
        status: StatusCode,
        retry_after: &'static str,
    ) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/api/oauth/usage",
                get(
                    move |State((hits, status, retry_after)): State<(
                        Arc<AtomicUsize>,
                        StatusCode,
                        &'static str,
                    )>| async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        let mut headers = axum::http::HeaderMap::new();
                        headers.insert("retry-after", retry_after.parse().unwrap());
                        (status, headers, CLAUDE_BODY)
                    },
                ),
            )
            .with_state((hits.clone(), status, retry_after));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, hits)
    }

    /// Record one failed poll for `key`, and report the backoff it earned.
    #[track_caller]
    fn note_failure(cache: &UsageCache, key: &SubKey, error: Error) -> Duration {
        cache.store(key.clone(), &Err(error));
        backoff_left(cache, key)
    }

    /// Whatever backoff `key` is currently serving out.
    #[track_caller]
    fn backoff_left(cache: &UsageCache, key: &SubKey) -> Duration {
        cache
            .peek(key)
            .expect("an entry")
            .retry_in
            .expect("a failure carries a backoff")
    }

    /// A 429 as the provider layer hands it over, `retry_after` already filtered.
    fn rate_limited(retry_after: Option<u64>) -> Error {
        Error::upstream_after(
            429,
            "rate_limit_error",
            retry_after.map(Duration::from_secs),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_poll_backs_off_on_a_rising_schedule() {
        let cache = UsageCache::default();
        let key = SubKey::new(Provider::Claude, "flaky");
        let waits: Vec<Duration> = (0..UsageCache::BACKOFF.len() + 1)
            .map(|_| note_failure(&cache, &key, Error::upstream(500, "unwell")))
            .collect();

        assert!(
            waits.windows(2).all(|pair| pair[1] >= pair[0]),
            "the ladder must never fall: {waits:?}"
        );
        assert!(
            waits[0] < UsageCache::TTL,
            "a first blip is retried sooner than a healthy poll: {waits:?}"
        );
        assert_eq!(
            waits[waits.len() - 2],
            waits[waits.len() - 1],
            "and then holds at the cap"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_backs_off_further_than_a_generic_failure() {
        let unwell = UsageCache::default();
        let throttled = UsageCache::default();
        let key = SubKey::new(Provider::Claude, "compared");

        for _ in 0..4 {
            let generic = note_failure(&unwell, &key, Error::upstream(500, "unwell"));
            let limited = note_failure(&throttled, &key, rate_limited(None));
            assert!(
                limited > generic,
                "being told we ask too often must never buy a shorter wait: \
                 {limited:?} vs {generic:?}"
            );
        }
        assert!(
            note_failure(&throttled, &key, rate_limited(None)) >= UsageCache::TTL,
            "a 429 never waits less than a healthy poll's own TTL"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_after_larger_than_the_backoff_is_honoured_as_a_floor() {
        let cache = UsageCache::default();
        let key = SubKey::new(Provider::Claude, "patient");

        // Half an hour: past every rung, so a floor rather than a clamp.
        assert_eq!(
            note_failure(&cache, &key, rate_limited(Some(1800))),
            Duration::from_secs(1800)
        );
        assert!(
            note_failure(&cache, &key, rate_limited(Some(5))) > Duration::from_secs(5),
            "a retry-after under the computed backoff changes nothing"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_success_resets_the_ladder() {
        let cache = UsageCache::default();
        let key = SubKey::new(Provider::Claude, "recovering");

        let first = note_failure(&cache, &key, rate_limited(None));
        assert!(note_failure(&cache, &key, rate_limited(None)) > first);

        // A quota signal off a proxied response is proof the account answers.
        cache.observe(&key, Usage::default());
        let entry = cache.peek(&key).expect("an entry");
        assert_eq!(entry.retry_in, None, "a success carries no backoff");

        assert_eq!(
            note_failure(&cache, &key, rate_limited(None)),
            first,
            "the next failure starts the ladder over"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_is_per_sub_so_one_throttled_account_does_not_slow_a_healthy_one() {
        let cache = UsageCache::default();
        let throttled = SubKey::new(Provider::Claude, "throttled");
        let other = SubKey::new(Provider::Claude, "other");

        for _ in 0..4 {
            note_failure(&cache, &throttled, rate_limited(None));
        }
        assert!(
            note_failure(&cache, &other, rate_limited(None)) < backoff_left(&cache, &throttled),
            "a second account starts at the bottom of its own ladder"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_sub_inside_its_backoff_is_not_asked_again() {
        let cache = UsageCache::default();
        let key = SubKey::new(Provider::Claude, "waiting");
        let wait = note_failure(&cache, &key, rate_limited(None));

        tokio::time::advance(wait - Duration::from_secs(1)).await;
        assert!(
            cache.fresh(&key).is_some(),
            "still inside the backoff: the cached failure is served, no request"
        );

        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(
            cache.fresh(&key).is_none(),
            "the backoff elapsed; ask again"
        );
    }

    #[tokio::test]
    async fn a_live_429_with_retry_after_zero_still_backs_off() {
        // Verbatim what the Anthropic usage endpoint sends.
        let (base, hits) = throttling_upstream(StatusCode::TOO_MANY_REQUESTS, "0").await;
        let cache = UsageCache::with_bases(Bases::all(base));
        let s = sub(Provider::Claude, "throttled");
        let deadline = || Instant::now() + Duration::from_secs(5);

        let err = cache.get(&s, false, deadline()).await.unwrap_err();
        assert!(matches!(err, Error::Upstream { status: 429, .. }));
        let left = backoff_left(&cache, &s.key);
        assert!(
            left + Duration::from_secs(5) >= UsageCache::TTL,
            "`retry-after: 0` must not buy a faster poll than health: {left:?}"
        );

        // A second read inside the backoff is served from the cache.
        cache.get(&s, false, deadline()).await.unwrap_err();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_forced_refresh_bypasses_the_backoff_without_resetting_it() {
        let (base, hits) = fake_upstream(StatusCode::INTERNAL_SERVER_ERROR).await;
        let cache = UsageCache::with_bases(Bases::all(base));
        let s = sub(Provider::Claude, "impatient");

        cache
            .get(&s, false, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap_err();
        let first = backoff_left(&cache, &s.key);

        // A user tapping "refresh" gets their request sent...
        let scored = cache
            .refresh_all(&[(SubId(1), &s)], Instant::now() + Duration::from_secs(5))
            .await;
        assert!(scored[0].1.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert!(
            backoff_left(&cache, &s.key) > first,
            "...but does not get to start the ladder over"
        );
    }
}
