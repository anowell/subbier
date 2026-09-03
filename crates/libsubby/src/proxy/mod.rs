//! The proxy: one axum listener fronting both providers. One port serves both
//! onboarding snippets — `OPENAI_BASE_URL=…/v1` and `ANTHROPIC_BASE_URL=…` —
//! plus the `/codex/…`, `/anthropic/…` and `/pool/<name>/…` aliases. No token
//! and no body reaches a log line; only [`body_excerpt`] of an *error* body.

pub mod claude;
pub mod codex;
pub mod metrics;
pub mod sse;
pub mod transcript;

use std::collections::BTreeSet;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use jiff::Timestamp;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::auth::TokenManager;
use crate::balance::{
    PoolGate, Router as BalanceRouter, SelectError, SubStatus, UsageRound, UsageScorer,
};
use crate::config::PoolConfig;
use crate::error::{Error, Result};
use crate::model::{Provider, Sub, SubId, SubKey, Tokens, Usage};
use crate::snapshot::Handle;
use crate::store::db::{Db, ProxiedRequestRow};
use crate::store::transcripts::{Limits, TranscriptStore};
use crate::usage::{Bases, UsageCache};

use metrics::{InFlightGuard, Metrics};

/// What we call ourselves upstream, in `originator` and `user-agent`.
pub const ORIGINATOR: &str = "subbier";

/// Generous on purpose: an extended-thinking request runs for minutes with
/// nothing on the wire but SSE keep-alives, and a 30s default severs it.
pub const FORWARD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Largest request body we will buffer before forwarding. The Codex path
/// rewrites the body, so it cannot stream the request.
pub const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;

/// How much of a body may appear in a log line or an error message.
pub const BODY_EXCERPT_LIMIT: usize = 200;

/// Never forwarded: reqwest has already decompressed and de-chunked the body,
/// so these would describe bytes that no longer exist.
pub const STRIPPED_RESPONSE_HEADERS: [&str; 5] = [
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "keep-alive",
];

/// An OpenAI-shaped error body, which is what both CLIs expect.
#[must_use]
pub fn error_response(status: u16, message: impl fmt::Display) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = json!({
        "error": {
            "message": message.to_string(),
            "type": "subbier_error",
            "param": null,
            "code": null,
        }
    });
    (status, axum::Json(body)).into_response()
}

/// A body prefix safe to log or to quote in an error: at most
/// [`BODY_EXCERPT_LIMIT`] characters, on one line.
#[must_use]
pub fn body_excerpt(body: &str) -> String {
    let flat: String = body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = flat.trim();
    match trimmed.char_indices().nth(BODY_EXCERPT_LIMIT) {
        Some((cut, _)) => format!("{}…", &trimmed[..cut]),
        None => trimmed.to_owned(),
    }
}

/// Whether `content-type` is exactly `application/json` once parameters are
/// stripped. Strict, and checked first: the types a browser can send
/// cross-origin without a preflight must not reach a user's local proxy.
#[must_use]
pub fn is_json_media_type(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .is_some_and(|v| v == "application/json")
}

/// One routable subscription, as the proxy sees it. The engine owns discovery,
/// config and health and publishes the result here.
#[derive(Debug, Clone, PartialEq)]
pub struct SubEntry {
    pub id: SubId,
    pub sub: Sub,
    pub enabled: bool,
    /// Skipped indefinitely: waiting never fixes a dead refresh token.
    pub needs_login: bool,
}

impl SubEntry {
    #[must_use]
    pub fn new(id: SubId, sub: Sub) -> Self {
        Self {
            id,
            sub,
            enabled: true,
            needs_login: false,
        }
    }

    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    #[must_use]
    pub fn needs_login(mut self, needs_login: bool) -> Self {
        self.needs_login = needs_login;
        self
    }

    #[must_use]
    pub fn key(&self) -> &SubKey {
        &self.sub.key
    }

    fn status(&self, metrics: &Metrics, usage: Option<&UsageCache>) -> SubStatus {
        // Whatever was last polled, stale or not: a pool ceiling is a coarse
        // gate, not worth a round-trip on the request path.
        let (session_pct, weekly_pct) = usage
            .and_then(|cache| cache.peek(&self.sub.key))
            .and_then(|entry| entry.usage.ok())
            .map(|u| (u.session.map(|w| w.pct), u.weekly.map(|w| w.pct)))
            .unwrap_or((None, None));
        SubStatus {
            sub: self.id,
            provider: self.sub.provider,
            enabled: self.enabled,
            needs_login: self.needs_login,
            proxied_in_flight: metrics.proxied_in_flight(self.id),
            proxied_requests_total: metrics.proxied_requests_total(self.id),
            session_pct,
            weekly_pct,
        }
    }
}

/// The live set of subs the proxy may route to, shared with the engine so that
/// a token refreshed on the request path is visible to the poller and back.
#[derive(Debug, Default)]
pub struct SubRegistry {
    entries: RwLock<Vec<SubEntry>>,
}

impl SubRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<SubEntry>> {
        self.entries.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<SubEntry>> {
        self.entries.write().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn replace(&self, entries: impl IntoIterator<Item = SubEntry>) {
        *self.write() = entries.into_iter().collect();
    }

    /// Add an entry, or replace the one with the same [`SubId`].
    pub fn upsert(&self, entry: SubEntry) {
        let mut entries = self.write();
        match entries.iter_mut().find(|e| e.id == entry.id) {
            Some(slot) => *slot = entry,
            None => entries.push(entry),
        }
    }

    /// Returns whether the entry was there.
    pub fn remove(&self, id: SubId) -> bool {
        let mut entries = self.write();
        let before = entries.len();
        entries.retain(|e| e.id != id);
        entries.len() != before
    }

    #[must_use]
    pub fn get(&self, id: SubId) -> Option<SubEntry> {
        self.read().iter().find(|e| e.id == id).cloned()
    }

    /// A [`SubKey`] is stable across restarts and a [`SubId`] is not, so
    /// anything outliving the process refers to a sub by key and resolves it here.
    #[must_use]
    pub fn id_of(&self, key: &SubKey) -> Option<SubId> {
        self.read().iter().find(|e| e.key() == key).map(|e| e.id)
    }

    #[must_use]
    pub fn of_provider(&self, provider: Provider) -> Vec<SubEntry> {
        self.read()
            .iter()
            .filter(|e| e.sub.provider == provider)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.read().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    pub fn set_enabled(&self, id: SubId, enabled: bool) {
        if let Some(entry) = self.write().iter_mut().find(|e| e.id == id) {
            entry.enabled = enabled;
        }
    }

    pub fn set_needs_login(&self, id: SubId, needs_login: bool) {
        if let Some(entry) = self.write().iter_mut().find(|e| e.id == id) {
            entry.needs_login = needs_login;
        }
    }

    /// Write refreshed tokens back, returning the updated sub so the caller can
    /// persist it. The vendor's own credential file is never written.
    pub fn store_tokens(&self, id: SubId, tokens: Tokens) -> Option<Sub> {
        let mut entries = self.write();
        let entry = entries.iter_mut().find(|e| e.id == id)?;
        entry.sub.credentials.tokens = tokens;
        Some(entry.sub.clone())
    }

    /// The subs a pool admits, by id. Resolution is by email, label, full key
    /// or bare account id — whichever the user wrote.
    #[must_use]
    pub fn resolve_pool(&self, pool: &PoolConfig) -> BTreeSet<SubId> {
        self.read()
            .iter()
            .filter(|e| pool.matches(&e.sub.key, &e.sub.label, e.sub.credentials.email.as_deref()))
            .map(|e| e.id)
            .collect()
    }

    /// `usage` fills in the percentages a pool ceiling tests against; pass
    /// `None` where no ceiling can apply.
    #[must_use]
    pub fn statuses(
        &self,
        provider: Provider,
        metrics: &Metrics,
        usage: Option<&UsageCache>,
    ) -> Vec<SubStatus> {
        self.read()
            .iter()
            .filter(|e| e.sub.provider == provider)
            .map(|e| e.status(metrics, usage))
            .collect()
    }
}

/// Called with the sub whose tokens the request path just refreshed.
pub type TokensPersisted = Arc<dyn Fn(&Sub) + Send + Sync>;

/// Everything the proxy needs, shared by `Arc` across every request.
pub struct ProxyState {
    /// Port 0 means "any free port"; the real one comes back as
    /// [`ProxyHandle::local_addr`].
    pub bind: SocketAddr,
    /// When set, every request must present `proxy.key`.
    pub key: Option<String>,
    pub subs: Arc<SubRegistry>,
    pub router: Arc<BalanceRouter>,
    pub tokens: Arc<TokenManager>,
    pub usage: Arc<UsageCache>,
    /// A strict subset of account traffic; see [`metrics`].
    pub metrics: Arc<Metrics>,
    pub bases: Bases,
    /// `proxied_request` rows, if history is enabled.
    pub db: Option<Arc<Db>>,
    /// Backs `GET /status`.
    pub snapshot: Option<Handle>,
    pub on_tokens_refreshed: Option<TokensPersisted>,
    /// Held un-resolved: resolving at request time ([`ProxyState::pool_gate`])
    /// lets an account logged in a minute ago join its pool without a restart.
    pools: RwLock<Vec<PoolConfig>>,
    /// The `previous_response_id` chains of the Codex path, and with them the
    /// account that served each turn.
    pub transcripts: Arc<TranscriptStore>,
    codex_models: Mutex<codex::ModelCatalog>,
    last_error: Mutex<Option<String>>,
}

impl fmt::Debug for ProxyState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyState")
            .field("bind", &self.bind)
            .field("requires_key", &self.key.is_some())
            .field("subs", &self.subs.len())
            .field("bases", &self.bases)
            .finish_non_exhaustive()
    }
}

impl ProxyState {
    /// Fresh, unshared components and upstream bases read from the environment.
    #[must_use]
    pub fn new(bind: SocketAddr) -> Self {
        let bases = Bases::from_env();
        Self {
            bind,
            key: None,
            subs: Arc::new(SubRegistry::new()),
            router: Arc::new(BalanceRouter::default()),
            tokens: Arc::new(TokenManager::new()),
            usage: Arc::new(UsageCache::with_bases(bases.clone())),
            metrics: Arc::new(Metrics::new()),
            bases,
            db: None,
            snapshot: None,
            on_tokens_refreshed: None,
            transcripts: Arc::new(
                TranscriptStore::in_memory(Limits::default())
                    .expect("sqlite can always open an in-memory database"),
            ),
            codex_models: Mutex::new(codex::ModelCatalog::default()),
            last_error: Mutex::new(None),
            pools: RwLock::new(Vec::new()),
        }
    }

    #[must_use]
    pub fn with_pools(self, pools: Vec<PoolConfig>) -> Self {
        self.set_pools(pools);
        self
    }

    pub fn set_pools(&self, pools: Vec<PoolConfig>) {
        *self.pools.write().unwrap_or_else(PoisonError::into_inner) = pools;
    }

    /// An unknown name errors rather than quietly serving the whole proxy: a
    /// typo in a base URL must not hand an experiment every account.
    pub fn pool_gate(&self, name: &str) -> std::result::Result<PoolGate, SelectError> {
        let pools = self.pools.read().unwrap_or_else(PoisonError::into_inner);
        let pool = pools
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name.trim()))
            .ok_or_else(|| SelectError::UnknownPool {
                pool: name.to_owned(),
            })?;
        Ok(PoolGate {
            name: pool.name.clone(),
            members: Some(self.subs.resolve_pool(pool)),
            max_session_pct: pool.max_session_pct(),
            max_weekly_pct: pool.max_weekly_pct(),
        })
    }

    #[must_use]
    pub fn with_key(mut self, key: Option<String>) -> Self {
        self.key = key;
        self
    }

    #[must_use]
    pub fn with_subs(mut self, subs: Arc<SubRegistry>) -> Self {
        self.subs = subs;
        self
    }

    #[must_use]
    pub fn with_router(mut self, router: Arc<BalanceRouter>) -> Self {
        self.router = router;
        self
    }

    /// Shared with the engine so a refresh on the request path is deduplicated
    /// against one the poller started.
    #[must_use]
    pub fn with_tokens(mut self, tokens: Arc<TokenManager>) -> Self {
        self.tokens = tokens;
        self
    }

    #[must_use]
    pub fn with_usage(mut self, usage: Arc<UsageCache>) -> Self {
        self.usage = usage;
        self
    }

    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// The test seam: editing the process environment is a data race under
    /// edition 2024.
    #[must_use]
    pub fn with_bases(mut self, bases: Bases) -> Self {
        self.usage = Arc::new(UsageCache::with_bases(bases.clone()));
        self.bases = bases;
        self
    }

    /// Shared with the engine, which owns the file it is backed by.
    #[must_use]
    pub fn with_transcripts(mut self, transcripts: Arc<TranscriptStore>) -> Self {
        self.transcripts = transcripts;
        self
    }

    #[must_use]
    pub fn with_db(mut self, db: Option<Arc<Db>>) -> Self {
        self.db = db;
        self
    }

    #[must_use]
    pub fn with_snapshot(mut self, handle: Handle) -> Self {
        self.snapshot = Some(handle);
        self
    }

    #[must_use]
    pub fn with_token_persistence(mut self, persist: TokensPersisted) -> Self {
        self.on_tokens_refreshed = Some(persist);
        self
    }

    #[must_use]
    pub fn base(&self, provider: Provider) -> &str {
        self.bases.get(provider)
    }

    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn note_error(&self, message: impl fmt::Display) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(message.to_string());
    }

    pub(crate) fn note_success(&self) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    pub(crate) fn codex_models(&self) -> MutexGuard<'_, codex::ModelCatalog> {
        self.codex_models
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn persist_tokens(&self, id: SubId, tokens: Tokens) {
        if let Some(sub) = self.subs.store_tokens(id, tokens)
            && let Some(persist) = &self.on_tokens_refreshed
        {
            persist(&sub);
        }
    }

    pub(crate) fn scorer(&self, provider: Provider) -> CacheScorer<'_> {
        CacheScorer {
            cache: &self.usage,
            subs: self
                .subs
                .of_provider(provider)
                .into_iter()
                .map(|e| (e.id, e.sub))
                .collect(),
        }
    }

    /// Fire-and-forget: a full sqlite queue drops the row rather than stalling
    /// the request path.
    pub(crate) fn record_row(&self, row: ProxiedRequestRow) {
        if let Some(db) = &self.db {
            db.record_proxied_request(row);
        }
    }
}

/// Bridges [`UsageCache`] to the router's injected [`UsageScorer`].
///
/// No token refresh happens here: a 401 from the usage endpoint is a fetch
/// failure — unknown usage, ranked last, never a quarantine.
pub(crate) struct CacheScorer<'a> {
    cache: &'a UsageCache,
    subs: Vec<(SubId, Sub)>,
}

impl UsageScorer for CacheScorer<'_> {
    fn usage_round<'a>(&'a self, ids: &'a [SubId], deadline: Duration) -> UsageRound<'a> {
        Box::pin(async move {
            let pairs: Vec<(SubId, &Sub)> = ids
                .iter()
                .filter_map(|id| {
                    self.subs
                        .iter()
                        .find(|(known, _)| known == id)
                        .map(|(known, sub)| (*known, sub))
                })
                .collect();
            let deadline = tokio::time::Instant::now() + deadline;
            let scored = self.cache.score_all(&pairs, deadline).await;
            ids.iter()
                .map(|id| {
                    scored
                        .iter()
                        .find(|(scored_id, _)| scored_id == id)
                        .and_then(|(_, usage)| usage.as_ref().ok().cloned())
                })
                .collect()
        })
    }
}

/// `session-id` and `x-client-request-id` carry this same value, regenerated
/// per attempt — including the 401 retry against the same sub, so a retry is
/// never mistaken upstream for a duplicate.
#[must_use]
pub fn new_request_id() -> String {
    uuid::Uuid::new_v4().hyphenated().to_string()
}

#[must_use]
pub fn passthrough_headers(upstream: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::with_capacity(upstream.len());
    for (name, value) in upstream {
        if STRIPPED_RESPONSE_HEADERS
            .iter()
            .any(|stripped| name.as_str().eq_ignore_ascii_case(stripped))
        {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            headers.append(name, value);
        }
    }
    headers
}

/// Holds the in-flight gauge up and writes the `proxied_request` row. Both
/// obligations end when the client has the last byte, so a streaming response
/// moves this into the body stream.
pub(crate) struct RequestRecord {
    state: Arc<ProxyState>,
    /// The sub *this attempt* is served by, captured once so every counter this
    /// record touches lands on the same account the row does.
    id: SubId,
    sub: SubKey,
    provider: Provider,
    route: String,
    /// The pool whose URL served this request, if any.
    pool: Option<String>,
    started: std::time::Instant,
    status: u16,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    guard: Option<InFlightGuard>,
}

impl RequestRecord {
    pub(crate) fn new(
        state: Arc<ProxyState>,
        id: SubId,
        sub: SubKey,
        provider: Provider,
        route: impl Into<String>,
    ) -> Self {
        let route = route.into();
        let pool = pool_from_path(&route).map(str::to_owned);
        let guard = state.metrics.in_flight(id, pool.as_deref());
        Self {
            state,
            id,
            sub,
            provider,
            route,
            pool,
            started: std::time::Instant::now(),
            status: 0,
            input_tokens: None,
            output_tokens: None,
            guard: Some(guard),
        }
    }

    pub(crate) fn set_status(&mut self, status: u16) {
        self.status = status;
    }

    /// The sub is deliberately not a parameter: a caller holding a more
    /// recently selected `SubId` cannot post another account's tokens here.
    pub(crate) fn set_tokens(&mut self, input: Option<u64>, output: Option<u64>) {
        if input.is_none() && output.is_none() {
            return;
        }
        self.input_tokens = input.map(|v| v.min(u64::from(u32::MAX)) as u32);
        self.output_tokens = output.map(|v| v.min(u64::from(u32::MAX)) as u32);
        self.state.metrics.record_tokens(
            self.id,
            self.pool.as_deref(),
            input.unwrap_or(0),
            output.unwrap_or(0),
            Timestamp::now(),
        );
    }
}

impl Drop for RequestRecord {
    fn drop(&mut self) {
        // Drop the gauge first: the request is over the moment we are.
        self.guard.take();
        let duration_ms = u32::try_from(self.started.elapsed().as_millis()).unwrap_or(u32::MAX);
        self.state.record_row(ProxiedRequestRow {
            ts: Timestamp::now(),
            sub: self.sub.clone(),
            provider: self.provider,
            route: std::mem::take(&mut self.route),
            status: self.status,
            duration_ms,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
        });
    }
}

/// A running proxy. Dropping it asks the server to shut down;
/// [`ProxyHandle::shutdown`] waits for it to finish.
#[derive(Debug)]
pub struct ProxyHandle {
    /// The address actually bound: with `bind`'s port 0, the one the OS chose.
    pub local_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl ProxyHandle {
    #[must_use]
    pub fn base_url(&self) -> String {
        let ip = self.local_addr.ip();
        if ip.is_ipv6() {
            format!("http://[{}]:{}", ip, self.local_addr.port())
        } else {
            format!("http://{}:{}", ip, self.local_addr.port())
        }
    }

    /// What the user sets `OPENAI_BASE_URL` to: `codex` appends `/responses`.
    #[must_use]
    pub fn openai_base_url(&self) -> String {
        format!("{}/v1", self.base_url())
    }

    /// What the user sets `ANTHROPIC_BASE_URL` to: `claude` appends
    /// `/v1/messages`, so this one carries **no** `/v1`.
    #[must_use]
    pub fn anthropic_base_url(&self) -> String {
        self.base_url()
    }

    /// `OPENAI_BASE_URL` for one pool: `http://127.0.0.1:8787/pool/moonshot/v1`.
    #[must_use]
    pub fn pool_openai_base_url(&self, pool: &str) -> String {
        format!("{}{POOL_PREFIX}{pool}/v1", self.base_url())
    }

    /// `ANTHROPIC_BASE_URL` for one pool. No `/v1`: `claude` appends its own.
    #[must_use]
    pub fn pool_anthropic_base_url(&self, pool: &str) -> String {
        format!("{}{POOL_PREFIX}{pool}", self.base_url())
    }

    pub async fn shutdown(mut self) {
        self.trigger();
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }

    fn trigger(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.trigger();
    }
}

/// Bind and start serving.
/// [`Error::Config`] when `bind` is not loopback and no `proxy.key` is set: an
/// open proxy holding somebody's OAuth tokens is not a thing we will start.
pub async fn serve(state: Arc<ProxyState>) -> Result<ProxyHandle> {
    if !is_loopback(state.bind.ip()) && state.key.is_none() {
        return Err(Error::config(format!(
            "proxy.bind is {}, which is not loopback, but proxy.key is unset: \
             set proxy.key to a shared secret, or bind to 127.0.0.1",
            state.bind
        )));
    }

    let listener = TcpListener::bind(state.bind).await?;
    let local_addr = listener.local_addr()?;
    let app = router(state);

    let (tx, rx) = oneshot::channel::<()>();
    let join = tokio::spawn(async move {
        let served = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
        if let Err(e) = served {
            tracing::error!(error = %e, "the proxy listener stopped");
        }
    });

    tracing::info!(%local_addr, "proxy listening");
    Ok(ProxyHandle {
        local_addr,
        shutdown: Some(tx),
        join: Some(join),
    })
}

fn is_loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
}

pub const POOL_PREFIX: &str = "/pool/";

/// The pool named in a request path, for a `/pool/<name>/…` URL.
///
/// ```
/// # use libsubby::proxy::pool_from_path;
/// assert_eq!(pool_from_path("/pool/moonshot/v1/responses"), Some("moonshot"));
/// assert_eq!(pool_from_path("/v1/responses"), None);
/// ```
#[must_use]
pub fn pool_from_path(path: &str) -> Option<&str> {
    path.strip_prefix(POOL_PREFIX)?
        .split('/')
        .next()
        .filter(|name| !name.is_empty())
}

/// The model id in a `…/models/<id>` path, which may itself contain slashes.
#[must_use]
fn model_from_path(path: &str) -> Option<&str> {
    path.split_once("/models/")
        .map(|(_, model)| model)
        .filter(|model| !model.is_empty())
}

pub fn router(state: Arc<ProxyState>) -> axum::Router {
    // axum 0.8 spells path parameters `/{id}`, never `/:id`; the old spelling
    // panics at router build time, not at `cargo check`.
    let mut app = axum::Router::new()
        .route("/healthz", get(healthz))
        .route("/status", get(status));

    // A pool is a base URL, so it has to accept everything the bare proxy does.
    let scopes = ["", "/pool/{pool}"];

    for scope in scopes {
        for prefix in ["", "/v1", "/codex", "/codex/v1"] {
            app = app
                .route(
                    &format!("{scope}{prefix}/responses"),
                    post(codex::responses),
                )
                .route(&format!("{scope}{prefix}/models"), get(codex::models))
                .route(
                    &format!("{scope}{prefix}/models/{{*model}}"),
                    get(codex::model),
                );
        }
    }

    for scope in scopes {
        for prefix in ["", "/v1", "/anthropic", "/anthropic/v1"] {
            app = app
                .route(&format!("{scope}{prefix}/messages"), post(claude::messages))
                .route(
                    &format!("{scope}{prefix}/messages/count_tokens"),
                    post(claude::count_tokens),
                );
        }
    }
    // `GET /v1/models` stays Codex's on the bare path; the Anthropic catalog is
    // reachable only under the explicit alias.
    for scope in scopes {
        for prefix in ["/anthropic", "/anthropic/v1"] {
            app = app.route(&format!("{scope}{prefix}/models"), get(claude::models));
        }
    }

    app.fallback(not_found)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_key,
        ))
        .with_state(state)
}

async fn healthz() -> Response {
    (StatusCode::OK, "ok").into_response()
}

async fn status(State(state): State<Arc<ProxyState>>) -> Response {
    match &state.snapshot {
        Some(handle) => axum::Json(handle.snapshot()).into_response(),
        None => error_response(503, "subbier is still starting up"),
    }
}

async fn not_found(request: Request) -> Response {
    error_response(
        404,
        format!(
            "subbier proxy: unknown route {} {}",
            request.method(),
            request.uri().path()
        ),
    )
}

/// The `proxy.key` gate. Either header works: `codex` sends
/// `Authorization: Bearer` and `claude` sends `x-api-key`.
async fn require_key(
    State(state): State<Arc<ProxyState>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(key) = state.key.as_deref() else {
        return next.run(request).await;
    };
    if presented_key(request.headers()).is_some_and(|presented| presented == key) {
        return next.run(request).await;
    }
    error_response(
        401,
        "set Authorization: Bearer <proxy.key>, or x-api-key: <proxy.key>",
    )
}

fn presented_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(bearer) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return Some(bearer.trim());
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

/// An early rejection: a finished [`Response`] to hand straight back. Boxed
/// because it is the rare arm of every `Result` it appears in.
pub(crate) type Rejection = Box<Response>;

pub(crate) fn reject(status: u16, message: impl fmt::Display) -> Rejection {
    Box::new(error_response(status, message))
}

pub(crate) async fn read_body(body: Body) -> std::result::Result<bytes::Bytes, Rejection> {
    axum::body::to_bytes(body, MAX_REQUEST_BODY)
        .await
        .map_err(|e| reject(413, format!("could not read the request body: {e}")))
}

/// The one request timeout on the forwarding path.
pub(crate) fn upstream(method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
    crate::http::client()
        .request(method, url)
        .timeout(FORWARD_TIMEOUT)
}

/// The pause before each resend; its length is the retry budget.
const SEND_BACKOFF: [Duration; 2] = [Duration::from_millis(50), Duration::from_millis(200)];

/// Send one already-buffered upstream request, resending it to the *same sub*
/// when the transport dies before any response header arrives — nothing is yet
/// known about the account, so rotating would spend a second one for nothing. A
/// timeout is never resent: the deadline was the answer.
pub(crate) async fn send_retrying<F, Fut>(
    provider: Provider,
    sub: &SubKey,
    mut send: F,
) -> reqwest::Result<reqwest::Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
{
    let mut attempt = 1usize;
    loop {
        let error = match send().await {
            Ok(response) => return Ok(response),
            Err(error) => error,
        };
        let backoff = if error.is_timeout() {
            None
        } else {
            SEND_BACKOFF.get(attempt - 1)
        };
        let Some(backoff) = backoff else {
            tracing::warn!(
                %provider,
                %sub,
                attempt,
                error = %crate::error::chain(&error),
                "upstream request failed",
            );
            return Err(error);
        };
        tracing::warn!(
            %provider,
            %sub,
            attempt,
            error = %crate::error::chain(&error),
            "upstream request failed before any response; resending to the same sub",
        );
        tokio::time::sleep(*backoff).await;
        attempt += 1;
    }
}

/// An empty string when the sub has no account id, never an omitted header.
#[must_use]
pub fn account_id_header(sub: &Sub) -> &str {
    sub.credentials.account_id.as_deref().unwrap_or("")
}

/// Refetch usage under a bounded deadline before quarantining, so the skip
/// lands on the real reset rather than the blind fallback.
pub(crate) async fn usage_for_exhaustion(state: &ProxyState, sub: &Sub) -> Option<Usage> {
    let deadline = tokio::time::Instant::now() + state.router.settings().usage_deadline;
    state.usage.get(sub, true, deadline).await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CredentialSource, Credentials};
    use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};

    pub(crate) fn test_sub(letter: &str) -> Sub {
        Sub {
            key: SubKey::new(Provider::Codex, letter),
            provider: Provider::Codex,
            label: format!("sub-{letter}"),
            credentials: Credentials {
                plan: None,
                account_id: Some(letter.to_owned()),
                email: None,
                tokens: Tokens {
                    access: format!("tok-{letter}"),
                    refresh: Some(format!("refresh-{letter}")),
                    expires_at: Some(Timestamp::now() + jiff::SignedDuration::from_hours(24)),
                },
                source: CredentialSource::Subbier,
            },
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn only_exactly_application_json_is_accepted() {
        assert!(is_json_media_type(&headers(&[(
            CONTENT_TYPE.as_str(),
            "application/json"
        )])));
        assert!(is_json_media_type(&headers(&[(
            CONTENT_TYPE.as_str(),
            "application/json; charset=utf-8"
        )])));
        assert!(is_json_media_type(&headers(&[(
            CONTENT_TYPE.as_str(),
            "APPLICATION/JSON"
        )])));

        for bad in [
            "text/plain",
            "multipart/form-data",
            "application/x-www-form-urlencoded",
            "application/json-whoops",
        ] {
            assert!(
                !is_json_media_type(&headers(&[(CONTENT_TYPE.as_str(), bad)])),
                "{bad} must be rejected"
            );
        }
        assert!(!is_json_media_type(&HeaderMap::new()), "missing => reject");
    }

    #[test]
    fn passthrough_strips_exactly_the_five_corrupting_headers() {
        let mut upstream = reqwest::header::HeaderMap::new();
        for (name, value) in [
            ("content-type", "text/event-stream"),
            ("content-length", "1234"),
            ("content-encoding", "gzip"),
            ("transfer-encoding", "chunked"),
            ("connection", "keep-alive"),
            ("keep-alive", "timeout=5"),
            ("x-request-id", "abc"),
        ] {
            upstream.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                reqwest::header::HeaderValue::from_static(value),
            );
        }
        let out = passthrough_headers(&upstream);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out["content-type"], "text/event-stream");
        assert_eq!(out["x-request-id"], "abc");
        for stripped in STRIPPED_RESPONSE_HEADERS {
            assert!(!out.contains_key(stripped), "{stripped} survived");
        }
    }

    #[test]
    fn a_body_excerpt_is_short_single_line_and_lossless_for_short_bodies() {
        assert_eq!(
            body_excerpt("  usage limit \n reached "),
            "usage limit   reached"
        );
        let long = "x".repeat(500);
        let excerpt = body_excerpt(&long);
        assert_eq!(excerpt.chars().count(), BODY_EXCERPT_LIMIT + 1);
        assert!(excerpt.ends_with('…'));
        // The cut must land on a character boundary.
        let multibyte = "é".repeat(500);
        assert_eq!(
            body_excerpt(&multibyte).chars().count(),
            BODY_EXCERPT_LIMIT + 1
        );
    }

    #[test]
    fn the_account_id_header_is_an_empty_string_when_absent() {
        let mut sub = test_sub("A");
        assert_eq!(account_id_header(&sub), "A");
        sub.credentials.account_id = None;
        assert_eq!(account_id_header(&sub), "", "empty, not omitted");
    }

    #[test]
    fn either_auth_header_spelling_presents_the_key() {
        assert_eq!(
            presented_key(&headers(&[(AUTHORIZATION.as_str(), "Bearer secret")])),
            Some("secret")
        );
        assert_eq!(
            presented_key(&headers(&[("x-api-key", "secret")])),
            Some("secret")
        );
        assert_eq!(
            presented_key(&headers(&[(AUTHORIZATION.as_str(), "Basic secret")])),
            None
        );
        assert_eq!(presented_key(&HeaderMap::new()), None);
    }

    #[test]
    fn the_registry_replaces_upserts_and_writes_tokens_back() {
        let registry = SubRegistry::new();
        registry.replace([
            SubEntry::new(SubId(1), test_sub("A")),
            SubEntry::new(SubId(2), test_sub("B")).needs_login(true),
        ]);
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.of_provider(Provider::Codex).len(), 2);
        assert_eq!(registry.of_provider(Provider::Claude).len(), 0);

        registry.set_enabled(SubId(2), false);
        let statuses = registry.statuses(Provider::Codex, &Metrics::new(), None);
        assert_eq!(statuses.len(), 2);
        assert!(statuses[0].enabled);
        assert!(!statuses[1].enabled);
        assert!(statuses[1].needs_login);

        let refreshed = Tokens {
            access: "new".into(),
            refresh: Some("rotated".into()),
            expires_at: None,
        };
        let updated = registry.store_tokens(SubId(1), refreshed).unwrap();
        assert_eq!(updated.credentials.tokens.access, "new");
        assert_eq!(
            registry
                .get(SubId(1))
                .unwrap()
                .sub
                .credentials
                .tokens
                .access,
            "new"
        );

        assert!(registry.remove(SubId(1)));
        assert!(!registry.remove(SubId(1)));
        assert_eq!(registry.len(), 1);
    }

    #[tokio::test]
    async fn a_non_loopback_bind_without_a_key_is_refused() {
        let state = Arc::new(ProxyState::new("0.0.0.0:0".parse().unwrap()));
        let err = serve(state).await.expect_err("must refuse to bind");
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        assert!(err.to_string().contains("proxy.key"), "{err}");
    }

    #[tokio::test]
    async fn the_server_binds_port_zero_and_reports_the_real_address() {
        let state = Arc::new(ProxyState::new("127.0.0.1:0".parse().unwrap()));
        let handle = serve(state).await.unwrap();
        assert_ne!(handle.local_addr.port(), 0);
        assert_eq!(
            handle.openai_base_url(),
            format!("http://127.0.0.1:{}/v1", handle.local_addr.port())
        );
        assert_eq!(handle.anthropic_base_url(), handle.base_url());

        let health = crate::http::client()
            .get(format!("{}/healthz", handle.base_url()))
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), 200);
        assert_eq!(health.text().await.unwrap(), "ok");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn the_key_gate_runs_before_routing() {
        let state = Arc::new(
            ProxyState::new("127.0.0.1:0".parse().unwrap()).with_key(Some("s3cret".into())),
        );
        let handle = serve(state).await.unwrap();
        let base = handle.base_url();
        let client = crate::http::client();

        // An unknown path, unauthorised, is 401 and NOT 404: a proxy that maps
        // out its own routing table for an unauthenticated caller is worse.
        let unknown = client.get(format!("{base}/nope")).send().await.unwrap();
        assert_eq!(unknown.status(), 401);

        let unknown = client
            .get(format!("{base}/nope"))
            .header("x-api-key", "s3cret")
            .send()
            .await
            .unwrap();
        assert_eq!(unknown.status(), 404);

        for request in [
            client.get(format!("{base}/healthz")).bearer_auth("s3cret"),
            client
                .get(format!("{base}/healthz"))
                .header("x-api-key", "s3cret"),
        ] {
            assert_eq!(request.send().await.unwrap().status(), 200);
        }
        assert_eq!(
            client
                .get(format!("{base}/healthz"))
                .bearer_auth("wrong")
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn status_is_503_until_the_engine_publishes_and_json_after() {
        let handle = serve(Arc::new(ProxyState::new("127.0.0.1:0".parse().unwrap())))
            .await
            .unwrap();
        let response = crate::http::client()
            .get(format!("{}/status", handle.base_url()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
        handle.shutdown().await;

        let (publisher, engine) = crate::snapshot::Publisher::new();
        publisher.publish(crate::snapshot::SnapshotData::default());
        let state = Arc::new(ProxyState::new("127.0.0.1:0".parse().unwrap()).with_snapshot(engine));
        let handle = serve(state).await.unwrap();
        let body: serde_json::Value = crate::http::client()
            .get(format!("{}/status", handle.base_url()))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["generation"], 1);
        assert!(body.get("subs").is_some(), "{body}");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn an_unknown_route_is_404_with_an_openai_shaped_error() {
        let state = Arc::new(ProxyState::new("127.0.0.1:0".parse().unwrap()));
        let handle = serve(state).await.unwrap();
        let response = crate::http::client()
            .post(format!("{}/v1/chat/completions", handle.base_url()))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"]["type"], "subbier_error");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown route"),
            "{body}"
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn dropping_the_handle_stops_the_listener() {
        let state = Arc::new(ProxyState::new("127.0.0.1:0".parse().unwrap()));
        let handle = serve(state).await.unwrap();
        let base = handle.base_url();
        drop(handle);
        // The graceful shutdown races us; poll until the port stops answering.
        for _ in 0..100 {
            if crate::http::client()
                .get(format!("{base}/healthz"))
                .send()
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the listener outlived its handle");
    }

    #[tokio::test]
    async fn a_send_that_never_reaches_a_server_spends_the_whole_budget() {
        let listener = TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);

        let calls = std::sync::atomic::AtomicUsize::new(0);
        let error = send_retrying(Provider::Codex, &SubKey::new(Provider::Codex, "a"), || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            crate::http::client().get(&url).send()
        })
        .await
        .expect_err("nothing is listening");

        assert!(!error.is_timeout(), "{error}");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_timed_out_send_is_never_resent() {
        let listener = TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        // Accept and answer nothing, so the request outlives its deadline.
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });

        let calls = std::sync::atomic::AtomicUsize::new(0);
        let error = send_retrying(Provider::Codex, &SubKey::new(Provider::Codex, "a"), || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            crate::http::client()
                .get(&url)
                .timeout(Duration::from_millis(100))
                .send()
        })
        .await
        .expect_err("the upstream never answers");

        assert!(error.is_timeout(), "{error}");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
