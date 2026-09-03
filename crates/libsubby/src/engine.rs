//! Owns the mutable state, runs the poll loop, applies [`Command`]s, starts and
//! stops the proxy, and publishes a [`Snapshot`](crate::Snapshot) on any visible
//! change. Allowance percentages are copied straight from the provider usage
//! APIs. The clock, poller, discovery and login flow are injected for tests.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use jiff::{SignedDuration, Timestamp};
use kdl::KdlValue;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::auth::{self, TokenManager};
use crate::balance::{self, Router, RouterSettings, SubStatus};
use crate::config::{self, Config};
use crate::model::{
    Credentials, Provider, Severity, Sub, SubId, Tokens, Usage, UsageWindow, WindowKind,
};
use crate::pace;
use crate::plan::PlanTier;
use crate::proxy::metrics::Metrics;
use crate::proxy::{self, ProxyHandle, ProxyState, SubEntry, SubRegistry};
use crate::render;
use crate::severity;
use crate::snapshot::{
    Alert, Command, Handle, LoginState, PoolView, ProxyView, Publisher, RoutingView, ScopedWindow,
    SettingsView, SnapshotData, SubHealth, SubView, WindowView,
};
use crate::store::transcripts::{Limits, TranscriptStore};
use crate::store::{self, creds, db::Db};
use crate::usage::{UsageCache, is_unauthorized};
use crate::{Error, Result};

/// How long proxy activity accumulates before a publish, so a burst of requests
/// is not a `SnapshotData` built per request.
const PUBLISH_DEBOUNCE: Duration = Duration::from_millis(200);

/// How long token rotations accumulate before `subs.json` is rewritten.
const TOKEN_PERSIST_DEBOUNCE: Duration = Duration::from_millis(500);

/// How often the proxy's counters are sampled; it has no event channel back to
/// the engine, so this is how its gauges reach a snapshot.
const PROXY_WATCH_INTERVAL: Duration = Duration::from_millis(500);

/// How often `config.kdl`'s mtime is checked. A pool can only be created by
/// hand, so a hand edit has to take effect on its own.
const CONFIG_WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// The shortest poll interval honoured; `tokio`'s interval timer panics on a
/// zero period.
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long before an adopted credential source is re-read for a signed-out sub.
/// Deliberately slow: on macOS the read runs `security`, which can prompt.
const RELOGIN_RECHECK_INTERVAL: SignedDuration = SignedDuration::from_secs(300);

/// How much of an account id a label keeps when there is no address; a whole
/// uuid is far wider than a menu row.
const ACCOUNT_ID_LABEL_CHARS: usize = 8;

/// Where "now" comes from, so a test can pin it.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Timestamp;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

#[derive(Debug, Clone)]
pub struct ManualClock(Arc<Mutex<Timestamp>>);

impl ManualClock {
    #[must_use]
    pub fn new(at: Timestamp) -> Self {
        Self(Arc::new(Mutex::new(at)))
    }

    pub fn advance(&self, by: SignedDuration) {
        let mut now = self.0.lock().expect("clock poisoned");
        *now += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        *self.0.lock().expect("clock poisoned")
    }
}

pub type PollRound<'a> = Pin<Box<dyn Future<Output = Vec<(SubId, Result<Usage>)>> + Send + 'a>>;

/// Refreshes allowance figures. Implementations must preserve the ordering they
/// were given, and report failure as `Err` — never an invented percentage.
pub trait UsagePoller: Send + Sync + 'static {
    /// Poll every sub under one shared `deadline`; `force` bypasses any cache.
    fn poll<'a>(
        &'a self,
        subs: &'a [(SubId, Sub)],
        force: bool,
        deadline: Duration,
    ) -> PollRound<'a>;
}

/// Polls through the shared [`UsageCache`], which keeps it warm for the request
/// path.
#[derive(Debug)]
pub struct CachePoller {
    cache: Arc<UsageCache>,
}

impl CachePoller {
    #[must_use]
    pub fn new(cache: Arc<UsageCache>) -> Self {
        Self { cache }
    }
}

impl UsagePoller for CachePoller {
    fn poll<'a>(
        &'a self,
        subs: &'a [(SubId, Sub)],
        force: bool,
        deadline: Duration,
    ) -> PollRound<'a> {
        Box::pin(async move {
            let pairs: Vec<(SubId, &Sub)> = subs.iter().map(|(id, sub)| (*id, sub)).collect();
            let deadline = Instant::now() + deadline;
            if force {
                self.cache.refresh_all(&pairs, deadline).await
            } else {
                self.cache.score_all(&pairs, deadline).await
            }
        })
    }
}

/// Zero-config account discovery.
pub trait Discovery: Send + Sync + 'static {
    /// Everything this machine is already logged in to. Blocking: it reads files
    /// and, on macOS, shells out to the Keychain.
    fn discover(&self) -> Vec<Sub>;
}

/// Discovery over `~/.codex/auth.json`, the macOS Keychain and
/// `~/.claude/.credentials.json`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemDiscovery;

impl Discovery for SystemDiscovery {
    fn discover(&self) -> Vec<Sub> {
        auth::discovery::discover()
            .into_iter()
            .map(auth::discovery::DiscoveredAccount::into_sub)
            .collect()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoDiscovery;

impl Discovery for NoDiscovery {
    fn discover(&self) -> Vec<Sub> {
        Vec::new()
    }
}

/// Receives the authorize URL as soon as the flow has one.
pub type UrlSink = Box<dyn FnOnce(&str) + Send>;

pub type LoginTask<'a> = Pin<Box<dyn Future<Output = Result<Credentials>> + Send + 'a>>;

/// Runs the OAuth+PKCE login for one provider. The URL arrives through `on_url`
/// because the engine publishes it long before the flow finishes.
pub trait LoginFlow: Send + Sync + 'static {
    /// Dropping the returned future cancels the flow.
    fn login<'a>(&'a self, provider: Provider, on_url: UrlSink) -> LoginTask<'a>;
}

/// Binds the loopback callback, opens the browser, exchanges the code.
#[derive(Debug, Clone, Copy, Default)]
pub struct BrowserLogin;

impl LoginFlow for BrowserLogin {
    fn login<'a>(&'a self, provider: Provider, on_url: UrlSink) -> LoginTask<'a> {
        Box::pin(async move {
            auth::login(provider, move |url| {
                if let Err(e) = auth::open_browser(url) {
                    tracing::warn!(error = %e, "could not open a browser; the URL is in the snapshot");
                }
                on_url(url);
            })
            .await
        })
    }
}

/// Something the engine learned from outside the command channel. Not public:
/// what a frontend could observe reaches it as a snapshot instead.
#[derive(Debug)]
enum Event {
    LoginUrl {
        provider: Provider,
        url: String,
    },
    LoginFinished {
        provider: Provider,
        result: std::result::Result<Box<Credentials>, String>,
    },
    /// The request path rotated a sub's tokens.
    TokensRefreshed(Box<Sub>),
    /// The proxy did something a snapshot would show. Coalesced.
    ProxyActivity,
}

/// What woke the loop; built in the `select!` arms so every handler runs with
/// the loop's borrows already released.
#[derive(Debug)]
enum Wake {
    Command(Option<Command>),
    Poll,
    Event(Event),
    Publish,
    Persist,
    WatchProxy,
    WatchConfig,
    Signal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Continue,
    Stop,
}

/// One sub, plus everything the engine remembers about polling it.
#[derive(Debug)]
struct SubRecord {
    id: SubId,
    sub: Sub,
    /// The last *successful* poll; a failure never overwrites it, which is what
    /// makes `SubHealth::Stale` show the previous numbers.
    usage: Option<Usage>,
    polled_at: Option<Timestamp>,
    failing_since: Option<Timestamp>,
    poll_error: Option<String>,
    /// `Some` once a refresh has permanently failed and only a re-login fixes it.
    needs_login: Option<String>,
    /// When the adopted source was last re-read; see [`RELOGIN_RECHECK_INTERVAL`].
    relogin_checked_at: Option<Timestamp>,
}

impl SubRecord {
    fn new(id: SubId, sub: Sub) -> Self {
        Self {
            id,
            sub,
            usage: None,
            polled_at: None,
            failing_since: None,
            poll_error: None,
            needs_login: None,
            relogin_checked_at: None,
        }
    }
}

/// The proxy counters as of the last sample. Compared, not published.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ProxySignature {
    proxied_requests_total: u64,
    proxied_in_flight: u32,
    current: [Option<SubId>; 2],
    pinned: Option<SubId>,
    exhausted: usize,
}

/// Build one with [`Engine::new`], then [`Engine::run`] it.
pub struct Engine {
    publisher: Publisher,
    state: State,
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("subs", &self.state.subs.len())
            .field("config", &self.state.config_path)
            .finish_non_exhaustive()
    }
}

struct State {
    clock: Arc<dyn Clock>,
    poller: Arc<dyn UsagePoller>,
    discovery: Arc<dyn Discovery>,
    login_flow: Arc<dyn LoginFlow>,

    config: Config,
    config_path: PathBuf,
    /// The config's mtime as last read or written here; `None` when the file
    /// does not exist, whose *appearance* is then a change like any other.
    config_seen: Option<SystemTime>,
    subs_path: PathBuf,
    configured_bind: SocketAddr,

    subs: Vec<SubRecord>,
    next_id: u32,
    /// Per-(sub, window) severity, updated on every evaluation so a band
    /// crossing notifies once and the next one re-arms.
    severities: HashMap<(SubId, WindowKind), Severity>,
    /// Crossings observed since the last publish. Drained into the snapshot.
    alerts: Vec<Alert>,
    login: Option<LoginState>,
    login_task: Option<JoinHandle<()>>,
    last_error: Option<String>,

    registry: Arc<SubRegistry>,
    router: Arc<Router>,
    tokens: Arc<TokenManager>,
    usage: Arc<UsageCache>,
    metrics: Arc<Metrics>,
    db: Option<Arc<Db>>,
    transcripts: Arc<TranscriptStore>,

    proxy: Option<ProxyHandle>,
    proxy_state: Option<Arc<ProxyState>>,
    proxy_signature: ProxySignature,

    handle: Handle,
    events: mpsc::UnboundedReceiver<Event>,
    event_tx: mpsc::UnboundedSender<Event>,

    /// When the debounced publish is due.
    dirty_at: Option<Instant>,
    /// When the debounced `subs.json` write is due.
    persist_at: Option<Instant>,
    shutdown_on_signal: bool,
    serve_proxy: bool,
}

impl Engine {
    /// Load config, load and discover subs — and start nothing. The returned
    /// [`Handle`] holds `Snapshot::empty()` until [`Engine::run`] publishes.
    ///
    /// A malformed `config.kdl` or `subs.json`; the absence of either is a fresh install.
    pub async fn new() -> Result<(Engine, Handle)> {
        let home = store::ensure_home()?;
        let config_path = home.join("config.kdl");
        // Just the retention window; `build` is what reports a malformed file.
        let retain_days = Config::load_from(&config_path).map_or(7, |c| c.history.retain_days);
        let db = match Db::open(&home.join("state.db"), retain_days) {
            Ok(db) => Some(Arc::new(db)),
            Err(e) => {
                tracing::warn!(error = %e, "could not open state.db; history is disabled");
                None
            }
        };
        let transcripts = match TranscriptStore::open(
            &home.join("transcripts.db"),
            Limits::default(),
        ) {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                tracing::warn!(error = %e, "could not open transcripts.db; chains are memory-only");
                None
            }
        };
        Engine::builder()
            .config_path(config_path)
            .subs_path(home.join("subs.json"))
            .db(db)
            .transcripts(transcripts)
            .build()
            .await
    }

    /// A builder, for tests and for frontends that want their own paths.
    #[must_use]
    pub fn builder() -> EngineBuilder {
        EngineBuilder::default()
    }

    /// Run until [`Command::Shutdown`] or a stop signal.
    pub async fn run(self) -> Result<()> {
        let Engine {
            mut publisher,
            mut state,
        } = self;

        tracing::info!(
            subs = state.subs.len(),
            config = %state.config_path.display(),
            poll_interval = %state.config.poll.interval,
            "engine starting"
        );

        state.sync_router_settings();
        state.sync_registry();
        state.publish(&publisher);

        state.sync_proxy().await;
        if state.proxy.is_some() {
            state.publish(&publisher);
        }

        let mut signals = Signals::new(state.shutdown_on_signal);
        let mut period = poll_period(state.config.poll.interval);
        let mut poll_timer = new_interval(period);
        let mut watch_timer = new_interval(PROXY_WATCH_INTERVAL);
        let mut config_timer = new_interval(CONFIG_WATCH_INTERVAL);

        loop {
            let dirty_at = state.dirty_at;
            let persist_at = state.persist_at;
            // An idle menu bar should not wake twice a second to sample nothing.
            let proxy_running = state.proxy.is_some();

            let wake = tokio::select! {
                cmd = publisher.commands().recv() => Wake::Command(cmd),
                _ = poll_timer.tick() => Wake::Poll,
                Some(event) = state.events.recv() => Wake::Event(event),
                () = wait_until(dirty_at) => Wake::Publish,
                () = wait_until(persist_at) => Wake::Persist,
                _ = watch_timer.tick(), if proxy_running => Wake::WatchProxy,
                _ = config_timer.tick() => Wake::WatchConfig,
                () = signals.recv() => Wake::Signal,
            };

            let step = match wake {
                Wake::Command(Some(cmd)) => state.apply(cmd, &publisher).await,
                // Unreachable while the engine holds its own `Handle`, but a
                // closed command channel can only ever mean "stop".
                Wake::Command(None) => {
                    tracing::info!("the command channel closed; shutting down");
                    Step::Stop
                }
                Wake::Poll => {
                    state.poll_round(false).await;
                    state.publish(&publisher);
                    Step::Continue
                }
                Wake::Event(event) => state.handle_event(event, &publisher).await,
                Wake::Publish => {
                    state.publish(&publisher);
                    Step::Continue
                }
                Wake::Persist => {
                    state.persist_subs();
                    Step::Continue
                }
                Wake::WatchProxy => {
                    state.sample_proxy();
                    Step::Continue
                }
                Wake::WatchConfig => {
                    if state.reload_config_if_edited().await {
                        state.publish(&publisher);
                    }
                    Step::Continue
                }
                Wake::Signal => {
                    tracing::info!("stop signal received; draining and shutting down");
                    Step::Stop
                }
            };

            if step == Step::Stop {
                break;
            }

            let wanted = poll_period(state.config.poll.interval);
            if wanted != period {
                tracing::info!(interval = ?wanted, "poll interval changed");
                period = wanted;
                poll_timer = new_interval(period);
            }
        }

        state.shutdown().await;
        state.publish(&publisher);
        tracing::info!("engine stopped");
        Ok(())
    }
}

/// Builds an [`Engine`] with explicit paths and injected dependencies. Anything
/// unset falls back to production, except sqlite: no history, in-memory chains.
#[derive(Default)]
pub struct EngineBuilder {
    config_path: Option<PathBuf>,
    subs_path: Option<PathBuf>,
    db: Option<Arc<Db>>,
    transcripts: Option<Arc<TranscriptStore>>,
    clock: Option<Arc<dyn Clock>>,
    poller: Option<Arc<dyn UsagePoller>>,
    tokens: Option<Arc<TokenManager>>,
    discovery: Option<Arc<dyn Discovery>>,
    login_flow: Option<Arc<dyn LoginFlow>>,
    shutdown_on_signal: Option<bool>,
    serve_proxy: Option<bool>,
}

impl fmt::Debug for EngineBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineBuilder")
            .field("config_path", &self.config_path)
            .field("subs_path", &self.subs_path)
            .finish_non_exhaustive()
    }
}

impl EngineBuilder {
    /// Where `config.kdl` lives. Default: `$SUBBIER_HOME/config.kdl`.
    #[must_use]
    pub fn config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path = Some(path.into());
        self
    }

    /// Where `subs.json` lives. Default: `$SUBBIER_HOME/subs.json`.
    #[must_use]
    pub fn subs_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.subs_path = Some(path.into());
        self
    }

    /// The history database, or `None` for no history.
    #[must_use]
    pub fn db(mut self, db: Option<Arc<Db>>) -> Self {
        self.db = db;
        self
    }

    /// Where the Codex path's `previous_response_id` chains live. Default: a
    /// store that dies with the process.
    #[must_use]
    pub fn transcripts(mut self, transcripts: Option<Arc<TranscriptStore>>) -> Self {
        self.transcripts = transcripts;
        self
    }

    /// Where "now" comes from. Default: [`SystemClock`].
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// What refreshes allowance figures. Default: [`CachePoller`] over the
    /// [`UsageCache`] the proxy uses.
    #[must_use]
    pub fn poller(mut self, poller: Arc<dyn UsagePoller>) -> Self {
        self.poller = Some(poller);
        self
    }

    /// What refreshes OAuth tokens. Default: a [`TokenManager`] aimed at the
    /// providers' real endpoints; a test points it at a local one.
    #[must_use]
    pub fn tokens(mut self, tokens: Arc<TokenManager>) -> Self {
        self.tokens = Some(tokens);
        self
    }

    /// Account discovery. Default: [`SystemDiscovery`].
    #[must_use]
    pub fn discovery(mut self, discovery: Arc<dyn Discovery>) -> Self {
        self.discovery = Some(discovery);
        self
    }

    /// The OAuth flow. Default: [`BrowserLogin`].
    #[must_use]
    pub fn login(mut self, login: Arc<dyn LoginFlow>) -> Self {
        self.login_flow = Some(login);
        self
    }

    /// Whether `run()` also stops on `SIGINT`/`SIGTERM`. Default: `true`.
    #[must_use]
    pub fn shutdown_on_signal(mut self, on: bool) -> Self {
        self.shutdown_on_signal = Some(on);
        self
    }

    /// Whether this engine may bind the listener at all. Default: `true`.
    /// `false` lets a short-lived process (`subbier status`) read the state
    /// without stealing the port from a long-running instance; only the
    /// listener is withheld, `proxy.enabled` still round-trips.
    #[must_use]
    pub fn serve_proxy(mut self, on: bool) -> Self {
        self.serve_proxy = Some(on);
        self
    }

    /// Load config and subs, and assemble the engine. Starts nothing.
    ///
    /// A malformed `config.kdl` or `subs.json`, or sqlite refusing the fallback
    /// in-memory transcript store.
    pub async fn build(self) -> Result<(Engine, Handle)> {
        let config_path = self
            .config_path
            .unwrap_or_else(|| store::home().join("config.kdl"));
        let subs_path = self
            .subs_path
            .unwrap_or_else(|| store::home().join("subs.json"));

        let config = Config::load_from(&config_path)?;
        let stored = creds::load_from(&subs_path)?;
        let transcripts = match self.transcripts {
            Some(store) => store,
            None => Arc::new(TranscriptStore::in_memory(Limits::default())?),
        };
        let discovery = self.discovery.unwrap_or_else(|| Arc::new(SystemDiscovery));

        let (publisher, handle) = Publisher::new();
        let (event_tx, events) = mpsc::unbounded_channel();
        let usage = Arc::new(UsageCache::new());
        let configured_bind = resolve_bind(&config.proxy.bind).unwrap_or_else(|e| {
            tracing::warn!(bind = %config.proxy.bind, error = %e, "unusable proxy.bind");
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787)
        });

        let mut state = State {
            clock: self.clock.unwrap_or_else(|| Arc::new(SystemClock)),
            poller: self
                .poller
                .unwrap_or_else(|| Arc::new(CachePoller::new(usage.clone()))),
            discovery,
            login_flow: self.login_flow.unwrap_or_else(|| Arc::new(BrowserLogin)),

            config,
            config_seen: file_mtime(&config_path),
            config_path,
            subs_path,
            configured_bind,

            subs: Vec::new(),
            next_id: 1,
            severities: HashMap::new(),
            alerts: Vec::new(),
            login: None,
            login_task: None,
            last_error: None,

            registry: Arc::new(SubRegistry::new()),
            router: Arc::new(Router::default()),
            tokens: self.tokens.unwrap_or_else(|| Arc::new(TokenManager::new())),
            usage,
            metrics: Arc::new(Metrics::new()),
            db: self.db,
            transcripts,

            proxy: None,
            proxy_state: None,
            proxy_signature: ProxySignature::default(),

            handle: handle.clone(),
            events,
            event_tx,

            dirty_at: None,
            persist_at: None,
            shutdown_on_signal: self.shutdown_on_signal.unwrap_or(true),
            serve_proxy: self.serve_proxy.unwrap_or(true),
        };

        // subs.json first: our own copy of a token is the fresher one.
        for sub in stored {
            state.adopt(sub);
        }
        let found = state.discover().await;
        let adopted = state.merge_discovered(found);
        if adopted > 0 {
            // Otherwise a first token refresh is the only thing that saves them.
            state.persist_subs();
        }
        // `run()` does this too, but a frontend may read the handle before then.
        state.sync_router_settings();
        state.sync_registry();

        tracing::info!(subs = state.subs.len(), adopted, "engine loaded");
        Ok((Engine { publisher, state }, handle))
    }
}

impl State {
    /// Build the whole snapshot from scratch; there is no partial-update path.
    fn build_snapshot(&self) -> SnapshotData {
        let now = self.clock.now();
        let warn = self.config.ui.warn_pct;
        let critical = self.config.ui.critical_pct;

        let statuses = self.sub_statuses();
        let eligible: Vec<SubId> = Provider::ALL
            .into_iter()
            .flat_map(|p| self.router.eligible(p, &statuses))
            .collect();
        let pinned = self.router.pinned();
        let current: [Option<SubId>; 2] = [
            self.router.current(Provider::Codex),
            self.router.current(Provider::Claude),
        ];

        let mut subs = Vec::with_capacity(self.subs.len());
        // `(worst pct, allowance weight)` per enabled sub, for `overall_pct`.
        let mut weighted: Vec<(f32, f32)> = Vec::with_capacity(self.subs.len());
        let mut worst = Severity::Ok;

        for record in &self.subs {
            let provider = record.sub.provider;
            let label = self.label_for(record);
            let enabled = self.is_enabled(record);

            let usage = record.usage.as_ref();
            let session = usage
                .and_then(|u| u.session)
                .map(|w| self.window_view(&w, now, warn, critical));
            let weekly = usage
                .and_then(|u| u.weekly)
                .map(|w| self.window_view(&w, now, warn, critical));
            let scoped: Vec<ScopedWindow> = usage
                .map(|u| {
                    u.scoped
                        .iter()
                        .map(|(name, w)| ScopedWindow {
                            name: name.clone(),
                            window: self.window_view(w, now, warn, critical),
                        })
                        .collect()
                })
                .unwrap_or_default();

            if enabled {
                for view in session
                    .iter()
                    .chain(weekly.iter())
                    .chain(scoped.iter().map(|s| &s.window))
                {
                    worst = worst.max(view.severity);
                }
            }

            let counters = self.metrics.proxied_counters(record.id);
            // Codex states its plan on the usage endpoint, Claude only in the
            // credential blob.
            let plan = usage
                .and_then(|u| u.plan.clone())
                .or_else(|| record.sub.credentials.plan.clone());
            let tier = PlanTier::resolve(provider, plan.as_deref());
            let weight = tier.weight_with(&self.config.plan_weights);
            if enabled {
                // The bars are read against the worst window, so that is what
                // the one aggregate number averages.
                let worst_pct = session
                    .iter()
                    .chain(weekly.iter())
                    .map(|w| w.pct)
                    .fold(f32::NEG_INFINITY, f32::max);
                if worst_pct.is_finite() {
                    weighted.push((worst_pct, weight));
                }
            }
            subs.push(SubView {
                id: record.id,
                key: record.sub.key.clone(),
                provider,
                label,
                plan,
                plan_tier: tier.id.to_string(),
                plan_weight: weight,
                source: record.sub.credentials.source.clone(),
                enabled,
                health: self.health_of(record),
                session,
                weekly,
                scoped,
                routing: RoutingView {
                    eligible: eligible.contains(&record.id),
                    active: pinned == Some(record.id)
                        || current[provider.index()] == Some(record.id),
                    proxied_requests_total: counters.proxied_requests_total,
                    last_proxied_at: counters.last_proxied_at,
                },
            });
        }

        // Needs every label built first, so it cannot fold into the loop above.
        disambiguate_labels(&mut subs);

        let last_error = self.last_error.clone();

        SnapshotData {
            // Overwritten by the publisher; it owns both fields.
            generation: 0,
            captured_at: now,
            pools: self.pool_views(&subs, now),
            overall_pct: crate::plan::weighted_pct(weighted),
            subs,
            proxy: self.proxy_view(),
            settings: self.settings_view(),
            worst,
            alerts: self.alerts.clone(),
            login: self.login.clone(),
            last_error,
        }
    }

    /// One allowance window, rendered straight from the provider usage API.
    fn window_view(
        &self,
        window: &UsageWindow,
        now: Timestamp,
        warn: f32,
        critical: f32,
    ) -> WindowView {
        WindowView {
            pct: window.pct,
            resets_at: window.resets_at,
            resets_in: window.resets_at.map(|at| now.duration_until(at)),
            severity: severity::severity_for(window.pct, warn, critical),
            projection: pace::project(window, now),
        }
    }

    /// One [`PoolView`] per configured pool, in file order. Membership is
    /// resolved per call, never cached: accounts come and go while it runs.
    fn pool_views(&self, subs: &[SubView], now: Timestamp) -> Vec<PoolView> {
        self.config
            .pools
            .iter()
            .map(|pool| {
                let members: Vec<&SubView> = self
                    .subs
                    .iter()
                    .filter(|record| {
                        pool.matches(
                            &record.sub.key,
                            &record.sub.label,
                            record.sub.credentials.email.as_deref(),
                        )
                    })
                    .filter_map(|record| subs.iter().find(|v| v.id == record.id))
                    .collect();

                // Router-eligible, plus the pool's own ceilings.
                let eligible = members
                    .iter()
                    .filter(|v| v.routing.eligible)
                    .filter(|v| {
                        v.session.is_none_or(|w| w.pct < pool.max_session_pct())
                            && v.weekly.is_none_or(|w| w.pct < pool.max_weekly_pct())
                    })
                    .map(|v| v.id)
                    .collect();

                PoolView {
                    name: pool.name.clone(),
                    provider: pool.provider,
                    members: members.iter().map(|v| v.id).collect(),
                    eligible,
                    max_session_pct: pool.max_session_pct(),
                    max_weekly_pct: pool.max_weekly_pct(),
                    openai_base_url: self
                        .proxy
                        .as_ref()
                        .map(|p| p.pool_openai_base_url(&pool.name)),
                    anthropic_base_url: self
                        .proxy
                        .as_ref()
                        .map(|p| p.pool_anthropic_base_url(&pool.name)),
                    // The pool's own endpoint, never its members summed: a
                    // member also serves the bare proxy and other pools.
                    proxied_in_flight: self.metrics.pool_proxied_in_flight(&pool.name),
                    proxied_tokens_1h: self.metrics.pool_proxied_tokens_1h(&pool.name, now),
                }
            })
            .collect()
    }

    fn proxy_view(&self) -> ProxyView {
        ProxyView {
            running: self.proxy.is_some(),
            configured_bind: self.configured_bind,
            listening: self.proxy.as_ref().map(|h| h.local_addr),
            openai_base_url: self.proxy.as_ref().map(ProxyHandle::openai_base_url),
            anthropic_base_url: self.proxy.as_ref().map(ProxyHandle::anthropic_base_url),
            requires_key: self.config.proxy.key.is_some(),
            proxied_in_flight: self.metrics.total_proxied_in_flight(),
            proxied_requests_total: self.metrics.total_proxied_requests(),
            proxied_tokens_1h: self.metrics.total_proxied_tokens_1h(self.clock.now()),
            last_error: self.proxy_state.as_ref().and_then(|s| s.last_error()),
            pid: Some(std::process::id()),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        }
    }

    fn settings_view(&self) -> SettingsView {
        SettingsView {
            proxy_enabled: self.config.proxy.enabled,
            auto_switch: self.config.proxy.auto_switch,
            strategy: self.config.proxy.strategy,
            sticky: self.config.proxy.effective_sticky(),
            providers_proxied: self.providers_proxied(),
            poll_interval: self.config.poll.interval,
            warn_pct: self.config.ui.warn_pct,
            critical_pct: self.config.ui.critical_pct,
            notifications_enabled: self.config.ui.notifications,
            menu_bar: self.config.ui.menu_bar,
            launch_at_login: self.config.ui.launch_at_login,
        }
    }

    /// Why this sub's numbers might not be trustworthy.
    fn health_of(&self, record: &SubRecord) -> SubHealth {
        if let Some(error) = &record.needs_login {
            return SubHealth::NeedsLogin {
                error: error.clone(),
            };
        }
        if let Some(until) = self.router.exhausted_until(record.id) {
            return SubHealth::Exhausted { until };
        }
        match (&record.poll_error, record.failing_since, &record.usage) {
            (Some(error), Some(since), Some(_)) => SubHealth::Stale {
                since,
                error: error.clone(),
            },
            (Some(_), _, None) => SubHealth::Unknown,
            (None, _, Some(_)) => SubHealth::Ok,
            _ => SubHealth::Unknown,
        }
    }

    /// The label a frontend renders; uniqueness is [`disambiguate_labels`]'s job.
    fn label_for(&self, record: &SubRecord) -> String {
        if let Some(configured) = self.config.sub_label(&record.sub.key) {
            return configured.to_owned();
        }
        let label = &record.sub.label;
        if label.contains('@') {
            return label.clone();
        }
        // `auth::default_label` fell back to the account id: a uuid, far too
        // wide for a one-line row.
        if record.sub.credentials.account_id.as_deref() == Some(label.as_str()) {
            return label.chars().take(ACCOUNT_ID_LABEL_CHARS).collect();
        }
        label.clone()
    }

    fn providers_proxied(&self) -> [bool; 2] {
        let mut proxied = [true; 2];
        proxied[Provider::Codex.index()] = self.config.proxy.codex;
        proxied[Provider::Claude.index()] = self.config.proxy.claude;
        proxied
    }

    fn is_enabled(&self, record: &SubRecord) -> bool {
        self.config.sub_enabled(&record.sub.key)
    }

    fn sub_statuses(&self) -> Vec<SubStatus> {
        self.subs
            .iter()
            .map(|record| {
                let counters = self.metrics.proxied_counters(record.id);
                let mut status = SubStatus::new(record.id, record.sub.provider)
                    .with_proxied(counters.proxied_in_flight, counters.proxied_requests_total);
                status.enabled = self.is_enabled(record);
                status.needs_login = record.needs_login.is_some();
                status
            })
            .collect()
    }

    /// Build and publish; alerts are drained, being the crossings observed
    /// since the previous snapshot.
    fn publish(&mut self, publisher: &Publisher) {
        let data = self.build_snapshot();
        self.alerts.clear();
        self.dirty_at = None;
        publisher.publish(data);
    }

    /// Ask for a publish soon. Repeated calls inside the window coalesce.
    fn mark_dirty(&mut self) {
        if self.dirty_at.is_none() {
            self.dirty_at = Some(Instant::now() + PUBLISH_DEBOUNCE);
        }
    }

    fn note_error(&mut self, message: impl fmt::Display) {
        let message = message.to_string();
        tracing::warn!(error = %message, "engine error");
        self.last_error = Some(message);
    }
}

impl State {
    /// Apply one command, then republish. Nothing returns anything: every
    /// outcome is visible in the snapshot published on the way out.
    async fn apply(&mut self, cmd: Command, publisher: &Publisher) -> Step {
        tracing::debug!(command = ?cmd, "applying command");
        match cmd {
            Command::SetProxyEnabled(on) => {
                self.config.proxy.enabled = on;
                self.write_config("proxy.enabled", KdlValue::Bool(on));
                self.sync_proxy().await;
            }
            Command::SetAutoSwitch(on) => {
                self.config.proxy.auto_switch = on;
                self.write_config("proxy.auto-switch", KdlValue::Bool(on));
                self.sync_router_settings();
            }
            Command::SetStrategy(kind) => {
                self.config.proxy.strategy = kind;
                self.write_config("proxy.strategy", KdlValue::String(kind.to_string()));
                self.sync_router_settings();
            }
            Command::SetSticky(sticky) => {
                self.config.proxy.sticky = sticky;
                // `#null` round-trips back to "unset", the third state.
                let value = sticky.map_or(KdlValue::Null, KdlValue::Bool);
                self.write_config("proxy.sticky", value);
                self.sync_router_settings();
            }
            Command::SetProviderProxied { provider, on } => {
                match provider {
                    Provider::Codex => self.config.proxy.codex = on,
                    Provider::Claude => self.config.proxy.claude = on,
                }
                self.write_config(&format!("proxy.{}", provider.id()), KdlValue::Bool(on));
                self.sync_router_settings();
            }
            Command::SetSubEnabled { sub, enabled } => self.set_sub_enabled(sub, enabled),
            Command::SetNotificationsEnabled(on) => {
                self.config.ui.notifications = on;
                self.write_config("ui.notifications", KdlValue::Bool(on));
            }
            Command::SetMenuBar(style) => {
                self.config.ui.menu_bar = style;
                self.write_config("ui.menu-bar", KdlValue::String(style.to_string()));
            }
            Command::SetLaunchAtLogin(on) => {
                self.config.ui.launch_at_login = on;
                self.write_config("ui.launch-at-login", KdlValue::Bool(on));
            }

            Command::Pin(sub) => {
                self.router.pin(sub);
                tracing::info!(pinned = ?sub, "pin changed");
            }
            Command::RefreshUsage { force } => self.poll_round(force).await,
            Command::ClearExhaustion(sub) => {
                self.router.clear_exhaustion(sub);
                tracing::info!(sub = %sub, "quarantine cleared by the user");
            }
            Command::Rediscover => {
                let found = self.discover().await;
                let adopted = self.merge_discovered(found);
                if adopted > 0 {
                    self.persist_subs();
                    self.sync_registry();
                    self.poll_round(false).await;
                }
                tracing::info!(adopted, subs = self.subs.len(), "rediscovery finished");
            }
            Command::Login(provider) => self.begin_login(provider),
            Command::CancelLogin => {
                if let Some(task) = self.login_task.take() {
                    // Dropping the flow releases the loopback port.
                    task.abort();
                    tracing::info!("login cancelled");
                }
                self.login = None;
            }
            Command::RemoveSub(sub) => self.remove_sub(sub),
            Command::ReloadConfig => self.reload_config().await,
            Command::Shutdown => return Step::Stop,
        }

        self.publish(publisher);
        Step::Continue
    }

    fn set_sub_enabled(&mut self, sub: SubId, enabled: bool) {
        let Some(record) = self.subs.iter().find(|r| r.id == sub) else {
            self.note_error(format!("no such sub: {sub}"));
            return;
        };
        let key = record.sub.key.clone();
        let entry = self.config.subs.entry(key.clone()).or_default();
        entry.enabled = enabled;
        self.write_config(
            &format!("sub.{}.enabled", key.as_str()),
            KdlValue::Bool(enabled),
        );
        self.registry.set_enabled(sub, enabled);
        tracing::info!(sub = %sub, key = %key, enabled, "sub toggled");
    }

    fn remove_sub(&mut self, sub: SubId) {
        let Some(index) = self.subs.iter().position(|r| r.id == sub) else {
            self.note_error(format!("no such sub: {sub}"));
            return;
        };
        let record = self.subs.remove(index);
        self.usage.invalidate(&record.sub.key);
        self.registry.remove(sub);
        self.metrics.forget(sub);
        self.router.clear_exhaustion(sub);
        if self.router.pinned() == Some(sub) {
            self.router.pin(None);
        }
        for provider in Provider::ALL {
            if self.router.current(provider) == Some(sub) {
                self.router.clear_current(provider);
            }
        }
        self.severities.retain(|(id, _), _| *id != sub);
        self.persist_subs();
        tracing::info!(sub = %sub, key = %record.sub.key, "sub removed");
    }

    /// Reload only if the file changed underneath us, returning whether it did.
    /// Own writes record the mtime they produced, so a menu click does not fire this.
    async fn reload_config_if_edited(&mut self) -> bool {
        let mtime = file_mtime(&self.config_path);
        if mtime == self.config_seen {
            return false;
        }
        tracing::info!(path = %self.config_path.display(), "config.kdl changed on disk");
        self.reload_config().await;
        true
    }

    async fn reload_config(&mut self) {
        self.config_seen = file_mtime(&self.config_path);
        match Config::load_from(&self.config_path) {
            Ok(config) => {
                tracing::info!(path = %self.config_path.display(), "config reloaded");
                self.config = config;
                self.last_error = None;
                self.apply_config().await;
            }
            Err(e) => self.note_error(format!(
                "could not reload {}: {e}",
                self.config_path.display()
            )),
        }
    }

    /// Push the current config into everything that caches a piece of it.
    async fn apply_config(&mut self) {
        match resolve_bind(&self.config.proxy.bind) {
            Ok(bind) => self.configured_bind = bind,
            Err(e) => self.note_error(e),
        }
        self.sync_router_settings();
        self.sync_registry();
        self.sync_pools();
        self.sync_proxy().await;
    }

    fn sync_pools(&self) {
        if let Some(state) = &self.proxy_state {
            state.set_pools(self.config.pools.clone());
        }
    }

    fn sync_router_settings(&self) {
        self.router.set_settings(RouterSettings {
            strategy: self.config.proxy.strategy,
            sticky: self.config.proxy.sticky,
            auto_switch: self.config.proxy.auto_switch,
            providers_proxied: self.providers_proxied(),
            usage_deadline: to_std(
                self.config.poll.usage_timeout,
                balance::DEFAULT_USAGE_DEADLINE,
            ),
        });
    }

    fn sync_registry(&self) {
        self.registry.replace(self.subs.iter().map(|record| {
            SubEntry::new(record.id, record.sub.clone())
                .enabled(self.is_enabled(record))
                .needs_login(record.needs_login.is_some())
        }));
    }

    fn write_config(&mut self, key: &str, value: KdlValue) {
        // A comment-preserving writer, never a reserialise: a menu click must
        // not eat the comments in someone's config file.
        if let Err(e) = config::write::set(&self.config_path, key, value.clone()) {
            self.note_error(format!(
                "could not write {key} to {}: {e}",
                self.config_path.display()
            ));
            return;
        }
        // Record our own mtime, so the watcher does not read this write back
        // as a hand edit.
        self.config_seen = file_mtime(&self.config_path);
        tracing::info!(key, value = %value, "config updated");
    }
}

impl State {
    /// One usage round: refresh tokens, poll enabled subs, record, alert.
    async fn poll_round(&mut self, force: bool) {
        self.recheck_signed_out().await;
        let targets: Vec<(SubId, Sub)> = self
            .subs
            .iter()
            .filter(|r| self.is_enabled(r) && r.needs_login.is_none())
            .map(|r| (r.id, r.sub.clone()))
            .collect();
        if targets.is_empty() {
            tracing::debug!("nothing to poll");
            return;
        }

        let targets = self.refresh_tokens(targets).await;
        if targets.is_empty() {
            return;
        }

        let deadline = to_std(
            self.config.poll.usage_timeout,
            balance::DEFAULT_USAGE_DEADLINE,
        );
        let results = self.poller.poll(&targets, force, deadline).await;
        let results = self.retry_unauthorized(&targets, results, deadline).await;
        let now = self.clock.now();
        let mut ok = 0usize;
        let mut failed = 0usize;

        for (id, result) in results {
            match result {
                Ok(usage) => {
                    ok += 1;
                    self.record_usage(id, usage, now);
                }
                Err(e) => {
                    failed += 1;
                    // The backoff it earned goes into the message, so a stale
                    // row can say why it is stale.
                    let message = match self.retry_in(id) {
                        Some(wait) => format!("{e}; retrying in {}", render::duration(wait)),
                        None => e.to_string(),
                    };
                    if let Some(record) = self.subs.iter_mut().find(|r| r.id == id) {
                        // Only the start of a failing run is a warning; the
                        // rest is the poller deliberately backing off.
                        if record.failing_since.is_none() {
                            tracing::warn!(sub = %id, error = %message, "usage poll failed");
                        } else {
                            tracing::debug!(sub = %id, error = %message, "usage poll still failing");
                        }
                        record.poll_error = Some(message);
                        record.failing_since.get_or_insert(now);
                    }
                }
            }
        }

        tracing::debug!(ok, failed, forced = force, "usage round finished");
    }

    /// Re-read the adopted source of every sub declared signed out, in case the
    /// user has since signed back in with `claude` or `codex`. Rate-limited per
    /// sub to [`RELOGIN_RECHECK_INTERVAL`]: on macOS the read can prompt.
    async fn recheck_signed_out(&mut self) {
        let now = self.clock.now();
        let due: Vec<(SubId, Sub)> = self
            .subs
            .iter()
            .filter(|r| r.needs_login.is_some() && self.is_enabled(r))
            .filter(|r| {
                r.relogin_checked_at
                    .is_none_or(|at| now.duration_since(at) >= RELOGIN_RECHECK_INTERVAL)
            })
            .map(|r| (r.id, r.sub.clone()))
            .collect();
        if due.is_empty() {
            return;
        }

        // Blocking: file reads, and `security` on macOS.
        let found = tokio::task::spawn_blocking(move || {
            due.into_iter()
                .map(|(id, sub)| (id, auth::discovery::reread(&sub)))
                .collect::<Vec<_>>()
        })
        .await;
        let found = match found {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!(error = %e, "re-reading adopted credentials panicked");
                return;
            }
        };

        let mut recovered = false;
        for (id, credentials) in found {
            let Some(record) = self.subs.iter_mut().find(|r| r.id == id) else {
                continue;
            };
            record.relogin_checked_at = Some(now);
            let Some(credentials) = credentials else {
                continue;
            };
            if credentials.tokens == record.sub.credentials.tokens {
                continue;
            }
            tracing::info!(key = %record.sub.key, "the source holds a new credential; adopting it and polling again");
            let tokens = credentials.tokens.clone();
            record.sub.credentials = credentials;
            record.needs_login = None;
            record.poll_error = None;
            record.failing_since = None;
            record.relogin_checked_at = None;
            self.registry.set_needs_login(id, false);
            self.registry.store_tokens(id, tokens);
            recovered = true;
        }
        if recovered {
            self.schedule_persist();
            self.mark_dirty();
        }
    }

    /// Refresh anything close to expiry, dropping whatever cannot be refreshed
    /// at all. Returns the subs still worth polling.
    async fn refresh_tokens(&mut self, targets: Vec<(SubId, Sub)>) -> Vec<(SubId, Sub)> {
        let tokens = self.tokens.clone();
        let refreshes = targets.into_iter().map(|(id, mut sub)| {
            let tokens = tokens.clone();
            async move {
                let outcome = tokens.ensure_fresh(&mut sub, false).await;
                (id, sub, outcome)
            }
        });
        let results = futures_util::future::join_all(refreshes).await;

        let mut alive = Vec::with_capacity(results.len());
        for (id, sub, outcome) in results {
            match outcome {
                Ok(changed) => {
                    if changed {
                        self.adopt_tokens(id, sub.credentials.tokens.clone());
                    }
                    alive.push((id, sub));
                }
                Err(e) if e.permanent => self.mark_needs_login(id, &e),
                Err(e) => {
                    tracing::debug!(sub = %id, error = %e, "transient refresh failure");
                    alive.push((id, sub));
                }
            }
        }
        alive
    }

    /// Poll again, once, behind a forced refresh, for every sub whose poll came
    /// back 401. Codex stops accepting a token long before its JWT `exp`, so
    /// `ensure_fresh` alone would refresh nothing. A refresh handing back the
    /// same access token is not retried: upstream rejected the credential.
    async fn retry_unauthorized(
        &mut self,
        targets: &[(SubId, Sub)],
        mut results: Vec<(SubId, Result<Usage>)>,
        deadline: Duration,
    ) -> Vec<(SubId, Result<Usage>)> {
        let rejected: Vec<(SubId, Sub)> = results
            .iter()
            .filter(|(_, result)| result.as_ref().err().is_some_and(is_unauthorized))
            .filter_map(|(id, _)| targets.iter().find(|(target, _)| target == id).cloned())
            .collect();
        if rejected.is_empty() {
            return results;
        }

        let tokens = self.tokens.clone();
        let refreshed =
            futures_util::future::join_all(rejected.into_iter().map(|(id, mut sub)| {
                let tokens = tokens.clone();
                async move {
                    let outcome = tokens.ensure_fresh(&mut sub, true).await;
                    (id, sub, outcome)
                }
            }))
            .await;

        let mut retry = Vec::new();
        for (id, sub, outcome) in refreshed {
            match outcome {
                Ok(true) => {
                    tracing::info!(sub = %id, "usage poll rejected the token; refreshed, retrying");
                    self.adopt_tokens(id, sub.credentials.tokens.clone());
                    retry.push((id, sub));
                }
                Ok(false) => {
                    tracing::debug!(sub = %id, "usage poll 401 but the token did not change");
                }
                Err(e) if e.permanent => self.mark_needs_login(id, &e),
                Err(e) => {
                    tracing::debug!(sub = %id, error = %e, "transient refresh failure after a 401");
                }
            }
        }
        if retry.is_empty() {
            return results;
        }

        // Forced: the cache is serving the 401 out of its backoff.
        for (id, result) in self.poller.poll(&retry, true, deadline).await {
            if let Some(slot) = results.iter_mut().find(|(polled, _)| *polled == id) {
                slot.1 = result;
            }
        }
        results
    }

    /// Take refreshed tokens into our own state and our own store — never
    /// `~/.codex/auth.json`, never the Keychain: adopted sources are read-only.
    fn adopt_tokens(&mut self, id: SubId, tokens: Tokens) {
        if let Some(record) = self.subs.iter_mut().find(|r| r.id == id) {
            record.sub.credentials.tokens = tokens.clone();
        }
        self.registry.store_tokens(id, tokens);
        self.schedule_persist();
    }

    fn mark_needs_login(&mut self, id: SubId, error: &auth::RefreshError) {
        let now = self.clock.now();
        if let Some(record) = self.subs.iter_mut().find(|r| r.id == id) {
            tracing::warn!(sub = %id, error = %error, "sub needs a new sign-in");
            record.needs_login = Some(error.message.clone());
            // The source was just read on the way to this verdict, so
            // re-reading it on the next tick would learn nothing.
            record.relogin_checked_at = Some(now);
        }
        self.registry.set_needs_login(id, true);
    }

    /// The usage cache's remaining backoff for this sub. `None` whenever it has
    /// none — including under an injected [`UsagePoller`].
    fn retry_in(&self, id: SubId) -> Option<SignedDuration> {
        let record = self.subs.iter().find(|r| r.id == id)?;
        let wait = self.usage.peek(&record.sub.key)?.retry_in?;
        SignedDuration::try_from(wait).ok()
    }

    /// Store a successful poll: numbers, history row, alerts, and the
    /// provider's own exhaustion verdict.
    fn record_usage(&mut self, id: SubId, usage: Usage, now: Timestamp) {
        let windows = windows_of(&usage);
        let exhausted = balance::is_exhausted(&usage);
        let Some(record) = self.subs.iter_mut().find(|r| r.id == id) else {
            return;
        };
        let key = record.sub.key.clone();
        record.usage = Some(usage.clone());
        record.polled_at = Some(now);
        record.failing_since = None;
        record.poll_error = None;

        if let Some(db) = &self.db {
            for (kind, window) in &windows {
                // Account-wide, straight from the provider API.
                db.record_allowance(&key, kind, window.pct, window.resets_at);
            }
        }

        if exhausted {
            let until = self.router.exhaust(id, Some(&usage));
            tracing::info!(
                target: "sub.exhausted",
                sub = %id,
                until = %until,
                cause = "usage-poll",
                "sub quarantined by its own allowance figures"
            );
        } else if usage.limit_reached == Some(false)
            && let Some(until) = self.router.exhausted_until(id)
        {
            // Only the provider's explicit all-clear ends a quarantine early: the
            // percentage cannot stand in, since the usage endpoint lags the verdict.
            self.router.clear_exhaustion(id);
            tracing::info!(
                target: "sub.recovered",
                sub = %id,
                was_until = %until,
                "quarantine lifted: the provider says the account is within its limits"
            );
        }

        self.evaluate_alerts(id, &windows);
    }

    /// Run the notification transition for every window of one sub. The
    /// severity is stored whether or not anything was emitted, which is what
    /// re-arms the next crossing after a drop.
    fn evaluate_alerts(&mut self, id: SubId, windows: &[(WindowKind, UsageWindow)]) {
        let warn = self.config.ui.warn_pct;
        let critical = self.config.ui.critical_pct;
        let notifications = self.config.ui.notifications;

        for (kind, window) in windows {
            let current = severity::severity_for(window.pct, warn, critical);
            let prev = self.severities.get(&(id, kind.clone())).copied();
            let transition = severity::notification_transition(prev, current);
            self.severities
                .insert((id, kind.clone()), transition.severity);

            let Some(notify) = transition.notify else {
                continue;
            };
            if !notifications {
                // The state map is still current, so toggling notifications
                // back on replays nothing.
                tracing::debug!(sub = %id, window = %kind, severity = %notify.as_str(), "alert suppressed");
                continue;
            }
            tracing::info!(sub = %id, window = %kind, severity = %notify.as_str(), pct = window.pct, "threshold crossed");
            self.alerts.push(Alert {
                sub: id,
                window: kind.clone(),
                severity: notify,
                pct: window.pct,
            });
        }
    }

    async fn discover(&self) -> Vec<Sub> {
        let discovery = self.discovery.clone();
        match tokio::task::spawn_blocking(move || discovery.discover()).await {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!(error = %e, "account discovery panicked");
                Vec::new()
            }
        }
    }

    /// Merge discovered accounts by [`SubKey`](crate::SubKey), keeping our own
    /// fresher tokens for anything already known. Returns how many were new.
    fn merge_discovered(&mut self, found: Vec<Sub>) -> usize {
        let mut adopted = 0;
        for sub in found {
            if let Some(record) = self.subs.iter_mut().find(|r| r.sub.key == sub.key) {
                tracing::debug!(key = %sub.key, "already known; keeping our own tokens");
                // The plan is the exception to "keep our own": only the
                // vendor's store states it, and it changes on an upgrade.
                if sub.credentials.plan.is_some()
                    && record.sub.credentials.plan != sub.credentials.plan
                {
                    tracing::info!(
                        key = %sub.key,
                        plan = ?sub.credentials.plan,
                        "took the plan from the vendor's own store"
                    );
                    record.sub.credentials.plan = sub.credentials.plan;
                    self.persist_at = Some(Instant::now());
                }
                continue;
            }
            tracing::info!(key = %sub.key, provider = %sub.provider, "adopted an account");
            self.adopt(sub);
            adopted += 1;
        }
        adopted
    }

    fn adopt(&mut self, sub: Sub) {
        let id = SubId(self.next_id);
        self.next_id += 1;
        self.subs.push(SubRecord::new(id, sub));
    }

    /// Ask for a `subs.json` write soon; a burst coalesces into one.
    fn schedule_persist(&mut self) {
        if self.persist_at.is_none() {
            self.persist_at = Some(Instant::now() + TOKEN_PERSIST_DEBOUNCE);
        }
    }

    fn persist_subs(&mut self) {
        self.persist_at = None;
        let subs: Vec<Sub> = self.subs.iter().map(|r| r.sub.clone()).collect();
        match creds::save_to(&self.subs_path, &subs) {
            Ok(()) => tracing::debug!(subs = subs.len(), "credentials saved"),
            Err(e) => self.note_error(format!("could not write {}: {e}", self.subs_path.display())),
        }
    }
}

impl State {
    /// Start an OAuth login. No reply channel: the URL and the outcome both
    /// arrive as events, and are published in the next snapshot.
    fn begin_login(&mut self, provider: Provider) {
        if let Some(task) = self.login_task.take() {
            task.abort();
        }
        self.login = None;

        let flow = self.login_flow.clone();
        let url_tx = self.event_tx.clone();
        let done_tx = self.event_tx.clone();
        tracing::info!(provider = %provider, "login started");
        self.login_task = Some(tokio::spawn(async move {
            let result = flow
                .login(
                    provider,
                    Box::new(move |url: &str| {
                        let _ = url_tx.send(Event::LoginUrl {
                            provider,
                            url: url.to_owned(),
                        });
                    }),
                )
                .await;
            let result = result.map(Box::new).map_err(|e| e.to_string());
            let _ = done_tx.send(Event::LoginFinished { provider, result });
        }));
    }

    async fn handle_event(&mut self, event: Event, publisher: &Publisher) -> Step {
        match event {
            Event::LoginUrl { provider, url } => {
                tracing::info!(provider = %provider, "waiting for the browser");
                self.login = Some(LoginState::AwaitingBrowser {
                    provider,
                    url,
                    started_at: self.clock.now(),
                });
                self.publish(publisher);
            }
            Event::LoginFinished { provider, result } => {
                self.login_task = None;
                match result {
                    Ok(credentials) => {
                        self.finish_login(provider, *credentials);
                        self.login = None;
                        self.poll_round(false).await;
                    }
                    Err(error) => {
                        tracing::warn!(provider = %provider, %error, "login failed");
                        self.login = Some(LoginState::Failed { provider, error });
                    }
                }
                self.publish(publisher);
            }
            Event::TokensRefreshed(sub) => {
                if let Some(record) = self.subs.iter_mut().find(|r| r.sub.key == sub.key) {
                    record.sub.credentials.tokens = sub.credentials.tokens.clone();
                    tracing::debug!(key = %sub.key, "tokens rotated on the request path");
                    self.schedule_persist();
                }
            }
            Event::ProxyActivity => self.mark_dirty(),
        }
        Step::Continue
    }

    fn finish_login(&mut self, provider: Provider, credentials: Credentials) {
        let sub = auth::to_sub(provider, credentials);
        if let Some(record) = self.subs.iter_mut().find(|r| r.sub.key == sub.key) {
            tracing::info!(key = %sub.key, "signed in again");
            record.sub.credentials = sub.credentials;
            record.needs_login = None;
            record.poll_error = None;
            record.failing_since = None;
        } else {
            tracing::info!(key = %sub.key, provider = %provider, "signed in");
            self.adopt(sub);
        }
        self.persist_subs();
        self.sync_registry();
    }

    /// Start, stop, or restart the listener to match the config.
    async fn sync_proxy(&mut self) {
        let wanted = self.serve_proxy && self.config.proxy.enabled;
        let running = self.proxy_state.as_ref().map(|s| (s.bind, s.key.clone()));
        let target = (self.configured_bind, self.config.proxy.key.clone());

        match (wanted, running) {
            (true, None) => self.start_proxy().await,
            (true, Some(current)) if current != target => {
                tracing::info!("proxy address or key changed; restarting");
                self.stop_proxy().await;
                self.start_proxy().await;
            }
            (false, Some(_)) => self.stop_proxy().await,
            _ => {}
        }
    }

    async fn start_proxy(&mut self) {
        if self.proxy.is_some() {
            return;
        }
        let tx = self.event_tx.clone();
        let state = Arc::new(
            ProxyState::new(self.configured_bind)
                .with_key(self.config.proxy.key.clone())
                .with_subs(self.registry.clone())
                .with_router(self.router.clone())
                .with_tokens(self.tokens.clone())
                .with_usage(self.usage.clone())
                .with_metrics(self.metrics.clone())
                .with_db(self.db.clone())
                .with_transcripts(self.transcripts.clone())
                .with_pools(self.config.pools.clone())
                .with_snapshot(self.handle.clone())
                // A token rotated on the request path reaches our own store
                // this way, never the vendor's file.
                .with_token_persistence(Arc::new(move |sub: &Sub| {
                    let _ = tx.send(Event::TokensRefreshed(Box::new(sub.clone())));
                })),
        );

        match proxy::serve(state.clone()).await {
            Ok(handle) => {
                tracing::info!(
                    listening = %handle.local_addr,
                    openai_base_url = %handle.openai_base_url(),
                    anthropic_base_url = %handle.anthropic_base_url(),
                    "proxy started"
                );
                self.proxy = Some(handle);
                self.proxy_state = Some(state);
                self.proxy_signature = ProxySignature::default();
            }
            Err(e) => self.note_error(format!("could not start the proxy: {e}")),
        }
    }

    async fn stop_proxy(&mut self) {
        if let Some(handle) = self.proxy.take() {
            let addr = handle.local_addr;
            handle.shutdown().await;
            tracing::info!(was_listening = %addr, "proxy stopped");
        }
        self.proxy_state = None;
        self.proxy_signature = ProxySignature::default();
    }

    /// Sample the proxy's counters; anything that moved arms a publish.
    fn sample_proxy(&mut self) {
        if self.proxy.is_none() {
            return;
        }
        let signature = ProxySignature {
            proxied_requests_total: self.metrics.total_proxied_requests(),
            proxied_in_flight: self.metrics.total_proxied_in_flight(),
            current: [
                self.router.current(Provider::Codex),
                self.router.current(Provider::Claude),
            ],
            pinned: self.router.pinned(),
            exhausted: self.router.exhaustions().len(),
        };
        if signature != self.proxy_signature {
            self.proxy_signature = signature;
            // Through the event channel, so proxy activity has one path to a
            // snapshot and one debounce however it was noticed.
            let _ = self.event_tx.send(Event::ProxyActivity);
        }
    }

    async fn shutdown(&mut self) {
        if let Some(task) = self.login_task.take() {
            task.abort();
        }
        self.login = None;
        self.stop_proxy().await;
        if self.persist_at.is_some() {
            self.persist_subs();
        }
    }
}

/// Every window of one usage report, tagged with the [`WindowKind`] the sqlite
/// `window` column and the severity state map are keyed by.
fn windows_of(usage: &Usage) -> Vec<(WindowKind, UsageWindow)> {
    let mut windows = Vec::with_capacity(2 + usage.scoped.len());
    if let Some(window) = usage.session {
        windows.push((WindowKind::Session, window));
    }
    if let Some(window) = usage.weekly {
        windows.push((WindowKind::Weekly, window));
    }
    for (name, window) in &usage.scoped {
        windows.push((WindowKind::Scoped(name.clone()), *window));
    }
    windows
}

/// Make every label unique *within its provider* — every frontend names the
/// provider beside the label, so one address on both is not a collision. The
/// plan is appended first (two ChatGPT workspaces differ by account id, not
/// email); whatever still collides gets a counter.
fn disambiguate_labels(subs: &mut [SubView]) {
    for group in colliding_labels(subs) {
        for i in group {
            if let Some(plan) = subs[i].plan.clone() {
                subs[i].label = format!("{} · {plan}", subs[i].label);
            }
        }
    }
    for group in colliding_labels(subs) {
        for (n, i) in group.into_iter().enumerate() {
            subs[i].label = format!("{} ({})", subs[i].label, n + 1);
        }
    }
}

/// Indices of the subs sharing a `(provider, label)`, each group ordered by
/// [`SubId`] so the numbering it feeds is stable.
fn colliding_labels(subs: &[SubView]) -> Vec<Vec<usize>> {
    let mut groups: HashMap<(Provider, &str), Vec<usize>> = HashMap::new();
    for (i, view) in subs.iter().enumerate() {
        groups
            .entry((view.provider, view.label.as_str()))
            .or_default()
            .push(i);
    }
    let mut colliding: Vec<Vec<usize>> = groups
        .into_values()
        .filter(|group| group.len() > 1)
        .collect();
    for group in &mut colliding {
        group.sort_by_key(|&i| subs[i].id);
    }
    colliding
}

/// `None` for a missing *or* unreadable file, on purpose: "it went away" is as
/// much a change as "it was saved".
fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// `host:port` -> a socket address, resolving DNS only for a bare hostname.
fn resolve_bind(bind: &str) -> Result<SocketAddr> {
    if let Ok(addr) = bind.parse::<SocketAddr>() {
        return Ok(addr);
    }
    let (host, port) = bind
        .rsplit_once(':')
        .ok_or_else(|| Error::config(format!("proxy.bind is {bind:?}, which is not host:port")))?;
    let port: u16 = port.parse().map_err(|_| {
        Error::config(format!(
            "proxy.bind is {bind:?}, whose port is not a number"
        ))
    })?;
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    }
    (host, port)
        .to_socket_addrs()
        .map_err(|e| {
            Error::config(format!(
                "proxy.bind is {bind:?}, which does not resolve: {e}"
            ))
        })?
        .next()
        .ok_or_else(|| Error::config(format!("proxy.bind is {bind:?}, which resolves to nothing")))
}

fn to_std(duration: SignedDuration, fallback: Duration) -> Duration {
    Duration::try_from(duration).unwrap_or(fallback)
}

/// The poll cadence, clamped; a negative interval falls back to
/// [`config::DEFAULT_POLL_INTERVAL`].
fn poll_period(interval: SignedDuration) -> Duration {
    to_std(interval, config::DEFAULT_POLL_INTERVAL.unsigned_abs()).max(MIN_POLL_INTERVAL)
}

fn new_interval(period: Duration) -> tokio::time::Interval {
    let mut timer = tokio::time::interval(period);
    // A slow poll round must not queue up a burst of catch-up ticks.
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    timer
}

/// A timer that never fires unless there is a deadline.
async fn wait_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The stop signals, registered once and held for the life of the loop: a
/// registration only sees signals that arrive after it is made, so re-creating
/// one per iteration leaves a window where a `SIGTERM` lands on nothing.
struct Signals {
    #[cfg(unix)]
    streams: Vec<tokio::signal::unix::Signal>,
    enabled: bool,
}

impl Signals {
    fn new(enabled: bool) -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            let mut streams = Vec::with_capacity(2);
            if enabled {
                for kind in [SignalKind::interrupt(), SignalKind::terminate()] {
                    match signal(kind) {
                        Ok(stream) => streams.push(stream),
                        Err(e) => {
                            tracing::warn!(error = %e, ?kind, "could not listen for this signal");
                        }
                    }
                }
            }
            Self { streams, enabled }
        }
        #[cfg(not(unix))]
        {
            Self { enabled }
        }
    }

    /// Resolve when the process is asked to stop.
    async fn recv(&mut self) {
        if !self.enabled {
            std::future::pending::<()>().await;
            return;
        }
        #[cfg(unix)]
        {
            if self.streams.is_empty() {
                std::future::pending::<()>().await;
                return;
            }
            let mut waits: futures_util::stream::FuturesUnordered<_> = self
                .streams
                .iter_mut()
                .map(tokio::signal::unix::Signal::recv)
                .collect();
            use futures_util::StreamExt as _;
            let _ = waits.next().await;
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CredentialSource, Credentials, MenuBarStyle, StrategyKind, SubKey, Tokens};
    use crate::store::tests_support::{TempDir, temp_dir};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ts(text: &str) -> Timestamp {
        text.parse().expect("timestamp")
    }

    /// A sub whose access token is valid for an hour, so no test reaches the
    /// network through `TokenManager::ensure_fresh`.
    fn sub(provider: Provider, account: &str, email: Option<&str>) -> Sub {
        Sub {
            key: SubKey::new(provider, account),
            provider,
            label: email.unwrap_or(account).to_owned(),
            credentials: Credentials {
                plan: None,
                account_id: Some(account.to_owned()),
                email: email.map(str::to_owned),
                tokens: Tokens {
                    access: "access-token".into(),
                    refresh: Some("refresh-token".into()),
                    expires_at: Some(Timestamp::now() + SignedDuration::from_hours(1)),
                },
                source: CredentialSource::Subbier,
            },
        }
    }

    fn usage_at(pct: f32) -> Usage {
        Usage {
            session: Some(UsageWindow::from_pct(pct)),
            ..Usage::default()
        }
    }

    #[derive(Debug, Default)]
    struct ScriptedPoller {
        rounds: Mutex<VecDeque<Vec<Result<Usage>>>>,
        calls: AtomicUsize,
    }

    impl ScriptedPoller {
        fn with(rounds: impl IntoIterator<Item = Vec<Result<Usage>>>) -> Arc<Self> {
            Arc::new(Self {
                rounds: Mutex::new(rounds.into_iter().collect()),
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl UsagePoller for ScriptedPoller {
        fn poll<'a>(
            &'a self,
            subs: &'a [(SubId, Sub)],
            _force: bool,
            _deadline: Duration,
        ) -> PollRound<'a> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let mut round = self
                .rounds
                .lock()
                .expect("poller poisoned")
                .pop_front()
                .unwrap_or_default();
            Box::pin(async move {
                subs.iter()
                    .map(|(id, _)| {
                        let result = if round.is_empty() {
                            Err(Error::other("no scripted result"))
                        } else {
                            round.remove(0)
                        };
                        (*id, result)
                    })
                    .collect()
            })
        }
    }

    /// A local token endpoint that always answers with `access`, counting calls.
    async fn fake_token_endpoint(access: &'static str) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(move || {
                seen.fetch_add(1, Ordering::SeqCst);
                async move {
                    axum::Json(serde_json::json!({
                        "access_token": access,
                        "refresh_token": "rotated-refresh-token",
                        "expires_in": 3600,
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (url, hits)
    }

    struct FixedDiscovery(Vec<Sub>);

    impl Discovery for FixedDiscovery {
        fn discover(&self) -> Vec<Sub> {
            self.0.clone()
        }
    }

    struct Harness {
        _dir: TempDir,
        config_path: PathBuf,
        subs_path: PathBuf,
    }

    impl Harness {
        fn new(name: &str, config: &str) -> Self {
            let dir = temp_dir(name);
            let config_path = dir.join("config.kdl");
            std::fs::write(&config_path, config).expect("write config");
            let subs_path = dir.join("subs.json");
            Self {
                _dir: dir,
                config_path,
                subs_path,
            }
        }

        fn builder(&self) -> EngineBuilder {
            Engine::builder()
                .config_path(&self.config_path)
                .subs_path(&self.subs_path)
                .db(None)
                .discovery(Arc::new(NoDiscovery))
                .shutdown_on_signal(false)
        }

        fn config_text(&self) -> String {
            std::fs::read_to_string(&self.config_path).expect("read config")
        }
    }

    /// The proxy is on by default, and a test must never race for port 8787.
    const PROXY_OFF: &str = "proxy {\n    enabled #false\n}\n";

    #[tokio::test]
    async fn a_bare_machine_produces_a_renderable_snapshot() {
        let dir = temp_dir("engine-bare");
        let (engine, handle) = Engine::builder()
            .config_path(dir.join("config.kdl"))
            .subs_path(dir.join("subs.json"))
            .db(None)
            .discovery(Arc::new(NoDiscovery))
            .build()
            .await
            .expect("no config file and no subs.json is a working install");

        assert!(
            handle.snapshot().is_empty(),
            "renderable before any publish"
        );

        let snap = engine.state.build_snapshot();
        assert!(snap.subs.is_empty());
        assert_eq!(snap.worst, Severity::Ok);
        assert!(!snap.proxy.running);
        assert!(snap.proxy.listening.is_none());
        assert_eq!(snap.settings, SettingsView::default());
        assert!(snap.login.is_none());
        assert!(snap.last_error.is_none());
    }

    #[tokio::test]
    async fn a_signed_out_adopted_sub_comes_back_when_the_source_does() {
        let harness = Harness::new("engine-relogin", PROXY_OFF);
        let source = harness.subs_path.with_file_name("auth.json");
        let auth = |refresh: &str| {
            serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": "at-1",
                    "refresh_token": refresh,
                    "account_id": "acct-1",
                },
            })
            .to_string()
        };
        std::fs::write(&source, auth("rt-dead")).expect("seed auth.json");
        let adopted = auth::discovery::codex_account_at(&source)
            .expect("an adopted account")
            .into_sub();
        creds::save_to(&harness.subs_path, &[adopted]).expect("seed subs.json");

        let clock = ManualClock::new(ts("2026-08-27T12:00:00Z"));
        let (mut engine, _handle) = harness
            .builder()
            .clock(Arc::new(clock.clone()))
            // Never reached: a signed-out sub is not polled, and the recovery
            // below costs no network.
            .poller(ScriptedPoller::with([]))
            .build()
            .await
            .expect("build");

        engine.state.subs[0].needs_login = Some("token endpoint returned 400".into());
        engine.state.recheck_signed_out().await;
        assert!(
            engine.state.subs[0].needs_login.is_some(),
            "the source still holds the same dead credential"
        );

        // `codex` is signed in again, and rotated the refresh token.
        std::fs::write(&source, auth("rt-live")).expect("rewrite auth.json");
        engine.state.recheck_signed_out().await;
        assert!(
            engine.state.subs[0].needs_login.is_some(),
            "re-reads are rate limited: the Keychain read can prompt"
        );

        clock.advance(RELOGIN_RECHECK_INTERVAL);
        engine.state.recheck_signed_out().await;
        assert_eq!(engine.state.subs[0].needs_login, None);
        assert_eq!(
            engine.state.subs[0]
                .sub
                .credentials
                .tokens
                .refresh
                .as_deref(),
            Some("rt-live")
        );
        assert!(matches!(
            engine.state.health_of(&engine.state.subs[0]),
            SubHealth::Unknown
        ));

        // Still read-only: the vendor's file is exactly what `codex` left.
        assert_eq!(std::fs::read_to_string(&source).unwrap(), auth("rt-live"));
    }

    #[tokio::test]
    async fn stored_subs_load_and_discovery_only_adds_new_keys() {
        let harness = Harness::new("engine-merge", PROXY_OFF);
        let mut stored = sub(Provider::Codex, "acct-1", Some("a@example.com"));
        stored.credentials.tokens.access = "ours-is-fresher".into();
        creds::save_to(&harness.subs_path, &[stored.clone()]).expect("seed subs.json");

        let mut adopted = sub(Provider::Codex, "acct-1", Some("a@example.com"));
        adopted.credentials.tokens.access = "the-vendors-older-copy".into();
        adopted.credentials.source = CredentialSource::Adopted {
            from: "~/.codex/auth.json".into(),
        };

        let (engine, _handle) = harness
            .builder()
            .discovery(Arc::new(FixedDiscovery(vec![
                adopted,
                sub(Provider::Claude, "acct-2", None),
            ])))
            .build()
            .await
            .expect("build");

        assert_eq!(engine.state.subs.len(), 2, "one merged, one adopted");
        let codex = &engine.state.subs[0];
        assert_eq!(codex.sub.key, stored.key);
        assert_eq!(
            codex.sub.credentials.tokens.access, "ours-is-fresher",
            "subbier keeps its own copy of an already-known account"
        );
    }

    #[tokio::test]
    async fn settings_round_trip_through_the_config_file_and_keep_comments() {
        let config = "\
// keep me: this comment is the whole point
proxy {
    enabled #false
    strategy \"lowest-usage\" // and this one
}
";
        let harness = Harness::new("engine-strategy", config);
        let (mut engine, handle) = harness.builder().build().await.expect("build");
        let publisher = &engine.publisher;

        let step = engine
            .state
            .apply(Command::SetStrategy(StrategyKind::RoundRobin), publisher)
            .await;
        assert_eq!(step, Step::Continue);
        engine
            .state
            .apply(Command::SetNotificationsEnabled(true), publisher)
            .await;
        engine
            .state
            .apply(Command::SetMenuBar(MenuBarStyle::Icon), publisher)
            .await;

        let text = harness.config_text();
        assert!(text.contains("// keep me: this comment is the whole point"));
        assert!(text.contains("// and this one"), "inline comment survived");
        assert!(text.contains("strategy \"round-robin\""), "{text}");
        assert!(text.contains("notifications #true"), "{text}");
        assert!(text.contains("menu-bar \"icon\""), "{text}");

        let snap = handle.snapshot();
        assert_eq!(snap.settings.strategy, StrategyKind::RoundRobin);
        assert!(snap.settings.notifications_enabled);
        assert_eq!(snap.settings.menu_bar, MenuBarStyle::Icon);
        assert_eq!(
            engine.state.router.settings().strategy,
            StrategyKind::RoundRobin,
            "the router follows the setting, not just the file"
        );
        let reloaded = Config::load_from(&harness.config_path).expect("reparse");
        assert_eq!(reloaded.proxy.strategy, StrategyKind::RoundRobin);
        assert!(reloaded.ui.notifications);
        assert_eq!(reloaded.ui.menu_bar, MenuBarStyle::Icon);
    }

    #[tokio::test]
    async fn clearing_sticky_round_trips_as_unset() {
        // A sticky-by-default strategy, so the fallback below has a `true`.
        let harness = Harness::new(
            "engine-sticky",
            "proxy {\n    enabled #false\n    strategy \"lowest-usage\"\n}\n",
        );
        let (mut engine, handle) = harness.builder().build().await.expect("build");
        let publisher = &engine.publisher;

        engine
            .state
            .apply(Command::SetSticky(Some(false)), publisher)
            .await;
        assert!(!handle.snapshot().settings.sticky);
        assert_eq!(
            Config::load_from(&harness.config_path)
                .expect("reparse")
                .proxy
                .sticky,
            Some(false)
        );

        engine
            .state
            .apply(Command::SetSticky(None), publisher)
            .await;
        assert_eq!(
            Config::load_from(&harness.config_path)
                .expect("reparse")
                .proxy
                .sticky,
            None,
            "unset in the menu is unset in the file"
        );
        assert!(
            handle.snapshot().settings.sticky,
            "the effective value falls back to the strategy default"
        );
    }

    #[tokio::test]
    async fn the_proxy_starts_and_stops_and_the_view_follows() {
        let harness = Harness::new(
            "engine-proxy",
            "proxy {\n    enabled #false\n    bind \"127.0.0.1:0\"\n}\n",
        );
        let (mut engine, handle) = harness.builder().build().await.expect("build");
        let publisher = &engine.publisher;

        engine
            .state
            .apply(Command::SetProxyEnabled(true), publisher)
            .await;
        let snap = handle.snapshot();
        assert!(snap.proxy.running);
        let addr = snap.proxy.listening.expect("a real bound address");
        assert_ne!(addr.port(), 0, "port 0 resolves to the real port");
        assert_eq!(
            snap.proxy.openai_base_url,
            Some(format!("http://{addr}/v1")),
            "the paste-ready OPENAI_BASE_URL"
        );
        assert!(
            tokio::net::TcpStream::connect(addr).await.is_ok(),
            "something is actually listening"
        );

        engine
            .state
            .apply(Command::SetProxyEnabled(false), publisher)
            .await;
        let snap = handle.snapshot();
        assert!(!snap.proxy.running);
        assert!(snap.proxy.listening.is_none());
        assert!(
            tokio::net::TcpStream::connect(addr).await.is_err(),
            "the listener really stopped"
        );
    }

    #[tokio::test]
    async fn a_failed_poll_goes_stale_over_the_previous_numbers() {
        let harness = Harness::new("engine-stale", PROXY_OFF);
        let clock = ManualClock::new(ts("2026-08-26T12:00:00Z"));
        let poller = ScriptedPoller::with([
            vec![Ok(usage_at(42.0))],
            vec![Err(Error::upstream(500, "upstream is unwell"))],
        ]);
        creds::save_to(&harness.subs_path, &[sub(Provider::Codex, "acct-1", None)])
            .expect("seed subs.json");

        let (mut engine, _handle) = harness
            .builder()
            .clock(Arc::new(clock.clone()))
            .poller(poller.clone())
            .build()
            .await
            .expect("build");

        engine.state.poll_round(false).await;
        let snap = engine.state.build_snapshot();
        assert_eq!(snap.subs[0].health, SubHealth::Ok);
        assert_eq!(snap.subs[0].session.expect("a window").pct, 42.0);

        clock.advance(SignedDuration::from_mins(1));
        engine.state.poll_round(false).await;
        let snap = engine.state.build_snapshot();

        match &snap.subs[0].health {
            SubHealth::Stale { since, error } => {
                assert_eq!(*since, ts("2026-08-26T12:01:00Z"));
                assert!(error.contains("500"), "{error}");
            }
            other => panic!("expected Stale, got {other:?}"),
        }
        assert_eq!(
            snap.subs[0].session.expect("a window").pct,
            42.0,
            "the previous percentage stays visible and is never re-derived"
        );
        assert_eq!(poller.calls(), 2);
    }

    #[tokio::test]
    async fn a_401_from_the_usage_poll_refreshes_the_token_and_retries() {
        // The token is nowhere near its stated expiry, so only the 401 can
        // prompt a refresh.
        let harness = Harness::new("engine-401-retry", PROXY_OFF);
        let poller = ScriptedPoller::with([
            vec![Err(Error::upstream(401, "token expired"))],
            vec![Ok(usage_at(3.0))],
        ]);
        let (token_url, hits) = fake_token_endpoint("fresh-access-token").await;
        creds::save_to(&harness.subs_path, &[sub(Provider::Codex, "acct-1", None)])
            .expect("seed subs.json");

        let (mut engine, _handle) = harness
            .builder()
            .poller(poller.clone())
            .tokens(Arc::new(TokenManager::with_token_urls(
                auth::TokenUrls::all(token_url),
            )))
            .build()
            .await
            .expect("build");

        engine.state.poll_round(false).await;

        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the 401 forces a refresh even though the token says it is valid"
        );
        assert_eq!(poller.calls(), 2, "and the sub is polled again, once");
        assert_eq!(
            engine.state.subs[0].sub.credentials.tokens.access, "fresh-access-token",
            "the rotated token is kept, so the next round starts from it"
        );

        let snap = engine.state.build_snapshot();
        assert_eq!(snap.subs[0].health, SubHealth::Ok);
        assert_eq!(snap.subs[0].session.expect("a window").pct, 3.0);
    }

    #[tokio::test]
    async fn a_401_is_not_retried_when_the_refresh_changes_nothing() {
        let harness = Harness::new("engine-401-unchanged", PROXY_OFF);
        let poller = ScriptedPoller::with([vec![Err(Error::upstream(401, "token expired"))]]);
        // The endpoint hands back the token the sub already holds.
        let (token_url, hits) = fake_token_endpoint("access-token").await;
        creds::save_to(&harness.subs_path, &[sub(Provider::Codex, "acct-1", None)])
            .expect("seed subs.json");

        let (mut engine, _handle) = harness
            .builder()
            .poller(poller.clone())
            .tokens(Arc::new(TokenManager::with_token_urls(
                auth::TokenUrls::all(token_url),
            )))
            .build()
            .await
            .expect("build");

        engine.state.poll_round(false).await;

        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            poller.calls(),
            1,
            "re-sending the credential upstream just rejected buys nothing"
        );
        assert_eq!(
            engine.state.build_snapshot().subs[0].health,
            SubHealth::Unknown
        );
    }

    #[tokio::test]
    async fn the_providers_all_clear_lifts_a_quarantine_early() {
        let harness = Harness::new("engine-unquarantine", PROXY_OFF);
        let full = Usage {
            session: Some(UsageWindow::from_pct(100.0)),
            ..Usage::default()
        };
        let quiet = Usage {
            session: Some(UsageWindow::from_pct(4.0)),
            limit_reached: None,
            ..Usage::default()
        };
        let all_clear = Usage {
            session: Some(UsageWindow::from_pct(0.0)),
            limit_reached: Some(false),
            ..Usage::default()
        };
        let poller = ScriptedPoller::with([vec![Ok(full)], vec![Ok(quiet)], vec![Ok(all_clear)]]);
        creds::save_to(&harness.subs_path, &[sub(Provider::Codex, "acct-1", None)])
            .expect("seed subs.json");

        let (mut engine, _handle) = harness
            .builder()
            .poller(poller)
            .build()
            .await
            .expect("build");

        engine.state.poll_round(false).await;
        assert!(
            engine.state.router.exhausted_until(SubId(1)).is_some(),
            "100% quarantines"
        );

        engine.state.poll_round(false).await;
        assert!(
            engine.state.router.exhausted_until(SubId(1)).is_some(),
            "a percentage that has dropped is not a verdict: the usage endpoint \
             lags the enforcement decision, which is what `limit_reached` is for"
        );

        engine.state.poll_round(false).await;
        assert_eq!(
            engine.state.router.exhausted_until(SubId(1)),
            None,
            "the provider itself says the account is within its limits"
        );
        assert_eq!(engine.state.build_snapshot().subs[0].health, SubHealth::Ok);
    }

    #[tokio::test]
    async fn a_failed_poll_never_synthesises_a_percentage() {
        let harness = Harness::new("engine-unknown", PROXY_OFF);
        let poller = ScriptedPoller::with([vec![Err(Error::other("no network"))]]);
        creds::save_to(&harness.subs_path, &[sub(Provider::Codex, "acct-1", None)])
            .expect("seed subs.json");

        let (mut engine, _handle) = harness
            .builder()
            .poller(poller)
            .build()
            .await
            .expect("build");

        engine.state.poll_round(false).await;
        let snap = engine.state.build_snapshot();
        assert_eq!(snap.subs[0].health, SubHealth::Unknown);
        assert!(
            snap.subs[0].session.is_none() && snap.subs[0].weekly.is_none(),
            "a fetch failure produces no window at all, not a 100"
        );
        assert_eq!(snap.worst, Severity::Ok);
        assert!(
            engine.state.router.exhausted_until(SubId(1)).is_none(),
            "a fetch failure must never quarantine"
        );
    }

    #[tokio::test]
    async fn alerts_fire_once_per_upward_crossing_and_re_arm() {
        let harness = Harness::new(
            "engine-alerts",
            "proxy {\n    enabled #false\n}\nui {\n    notifications #true\n}\n",
        );
        let poller = ScriptedPoller::with([
            vec![Ok(usage_at(95.0))], // first observation, already critical
            vec![Ok(usage_at(96.0))], // same band
            vec![Ok(usage_at(50.0))], // drop: silent, but re-arms
            vec![Ok(usage_at(80.0))], // upward crossing into warn
            vec![Ok(usage_at(95.0))], // upward crossing into critical
        ]);
        creds::save_to(&harness.subs_path, &[sub(Provider::Codex, "acct-1", None)])
            .expect("seed subs.json");

        let (mut engine, _handle) = harness
            .builder()
            .poller(poller)
            .build()
            .await
            .expect("build");

        let mut rounds = Vec::new();
        for _ in 0..5 {
            engine.state.poll_round(false).await;
            rounds.push(std::mem::take(&mut engine.state.alerts));
        }

        assert!(
            rounds[0].is_empty(),
            "starting up already critical is a baseline, not an alert"
        );
        assert!(rounds[1].is_empty(), "no repeat inside the same band");
        assert!(rounds[2].is_empty(), "a drop is silent");
        assert_eq!(rounds[3].len(), 1);
        assert_eq!(rounds[3][0].severity, Severity::Warn);
        assert_eq!(rounds[3][0].window, WindowKind::Session);
        assert_eq!(rounds[3][0].pct, 80.0);
        assert_eq!(rounds[4].len(), 1);
        assert_eq!(rounds[4][0].severity, Severity::Critical);
    }

    #[tokio::test]
    async fn notifications_off_suppresses_alerts_but_keeps_the_state_map() {
        let harness = Harness::new("engine-quiet", PROXY_OFF);
        let poller = ScriptedPoller::with([
            vec![Ok(usage_at(10.0))],
            vec![Ok(usage_at(95.0))], // crossing, suppressed
            vec![Ok(usage_at(96.0))],
        ]);
        creds::save_to(&harness.subs_path, &[sub(Provider::Codex, "acct-1", None)])
            .expect("seed subs.json");

        let (mut engine, _handle) = harness
            .builder()
            .poller(poller)
            .build()
            .await
            .expect("build");

        for _ in 0..3 {
            engine.state.poll_round(false).await;
            assert!(engine.state.alerts.is_empty(), "notifications are off");
        }
        assert_eq!(
            engine
                .state
                .severities
                .get(&(SubId(1), WindowKind::Session))
                .copied(),
            Some(Severity::Critical),
            "the state map keeps tracking, so toggling notifications on does \
             not replay an old crossing"
        );
    }

    #[tokio::test]
    async fn labels_are_disambiguated_within_a_provider() {
        let harness = Harness::new("engine-labels", PROXY_OFF);
        creds::save_to(
            &harness.subs_path,
            &[
                sub(Provider::Codex, "workspace-1", Some("shared@example.com")),
                sub(Provider::Codex, "workspace-2", Some("shared@example.com")),
                sub(Provider::Codex, "acct-3", Some("solo@example.com")),
                sub(Provider::Claude, "acct-4", Some("shared@example.com")),
                sub(Provider::Codex, "4575f150-aaaa-bbbb", None),
                sub(Provider::Codex, "4575f150-cccc-dddd", None),
                sub(Provider::Codex, "9e0c1122-eeee", None),
            ],
        )
        .expect("seed subs.json");
        let (mut engine, _handle) = harness.builder().build().await.expect("build");
        engine.state.subs[0].usage = Some(Usage {
            plan: Some("plus".into()),
            ..usage_at(10.0)
        });
        engine.state.subs[1].usage = Some(Usage {
            plan: Some("team".into()),
            ..usage_at(10.0)
        });
        engine.state.subs[2].usage = Some(Usage {
            plan: Some("plus".into()),
            ..usage_at(10.0)
        });

        let snap = engine.state.build_snapshot();
        assert_eq!(
            snap.subs[0].label, "shared@example.com · plus",
            "two workspaces behind one login are told apart by plan"
        );
        assert_eq!(snap.subs[1].label, "shared@example.com · team");
        assert_eq!(
            snap.subs[2].label, "solo@example.com",
            "an account that collides with nobody never grows a suffix"
        );
        assert_eq!(
            snap.subs[3].label, "shared@example.com",
            "every frontend prints the provider beside the label, so the same \
             address on two providers is not a collision"
        );
        // A uuid does not fit a one-line row; eight characters do, and the
        // collision pass finishes the job when eight are not enough.
        assert_eq!(snap.subs[4].label, "4575f150 (1)");
        assert_eq!(snap.subs[5].label, "4575f150 (2)");
        assert_eq!(snap.subs[6].label, "9e0c1122");
    }

    /// The plan is the one field discovery is authoritative for.
    #[tokio::test]
    async fn a_known_sub_takes_the_plan_but_keeps_its_tokens() {
        let harness = Harness::new("engine-plan-merge", PROXY_OFF);
        // A sub stored before `plan` existed: no plan, and a token of its own.
        let mut stored = sub(Provider::Claude, "acct-1", Some("a@example.com"));
        stored.credentials.plan = None;
        stored.credentials.tokens.access = "ours-is-fresher".into();
        creds::save_to(&harness.subs_path, &[stored.clone()]).expect("seed subs.json");

        let (mut engine, handle) = harness.builder().build().await.expect("build");
        let key = engine.state.subs[0].sub.key.clone();
        let provider = engine.state.subs[0].sub.provider;
        let ours = engine.state.subs[0].sub.credentials.tokens.access.clone();

        let mut discovered = stored.clone();
        discovered.credentials.plan = Some("max_20x".into());
        discovered.credentials.tokens.access = "a-token-from-the-vendor".into();

        assert_eq!(
            engine.state.merge_discovered(vec![discovered]),
            0,
            "nothing new"
        );
        assert_eq!(
            engine.state.subs[0].sub.credentials.plan.as_deref(),
            Some("max_20x")
        );
        assert_eq!(
            engine.state.subs[0].sub.credentials.tokens.access, ours,
            "our own token is still ours"
        );

        engine.state.publish(&engine.publisher);
        let snap = handle.snapshot();
        let view = snap.subs.iter().find(|s| s.provider == provider).unwrap();
        assert_eq!(view.plan.as_deref(), Some("max_20x"), "{key}");
        assert_eq!(view.plan_tier, "max-20x");
    }

    #[tokio::test]
    async fn the_overall_percentage_is_weighted_by_plan() {
        let harness = Harness::new("engine-overall-pct", PROXY_OFF);
        creds::save_to(
            &harness.subs_path,
            &[
                sub(Provider::Claude, "acct-1", Some("big@example.com")),
                sub(Provider::Claude, "acct-2", Some("small@example.com")),
            ],
        )
        .expect("seed subs.json");
        let (mut engine, handle) = harness.builder().build().await.expect("build");
        assert_eq!(engine.state.subs.len(), 2);

        // A big account barely touched, beside a small one nearly gone.
        engine.state.subs[0].sub.credentials.plan = Some("max_20x".into());
        engine.state.subs[1].sub.credentials.plan = Some("pro".into());
        engine.state.subs[0].usage = Some(usage_at(10.0));
        engine.state.subs[1].usage = Some(usage_at(90.0));

        engine.state.publish(&engine.publisher);
        let overall = handle.snapshot().overall_pct.expect("both accounts polled");

        assert!(
            overall < 20.0,
            "the big account dominates; an unweighted mean would be 50, got {overall}"
        );
    }

    #[tokio::test]
    async fn a_burst_of_proxy_events_produces_one_publish() {
        let harness = Harness::new("engine-debounce", PROXY_OFF);
        let (mut engine, handle) = harness.builder().build().await.expect("build");
        let publisher = &engine.publisher;
        engine.state.publish(publisher);
        let before = handle.snapshot().generation;

        for _ in 0..100 {
            engine
                .state
                .handle_event(Event::ProxyActivity, publisher)
                .await;
        }
        assert_eq!(
            handle.snapshot().generation,
            before,
            "100 proxy events published nothing on their own"
        );
        assert!(engine.state.dirty_at.is_some(), "one publish is armed");

        // What the loop's debounce arm does when the timer fires.
        engine.state.publish(publisher);
        assert_eq!(
            handle.snapshot().generation,
            before + 1,
            "100 events, one publish"
        );
        assert!(engine.state.dirty_at.is_none());
    }

    #[tokio::test]
    async fn a_login_publishes_its_url_with_no_reply_channel() {
        struct FakeLogin;
        impl LoginFlow for FakeLogin {
            fn login<'a>(&'a self, provider: Provider, on_url: UrlSink) -> LoginTask<'a> {
                assert_eq!(provider, Provider::Claude);
                Box::pin(async move {
                    on_url("https://auth.example.com/authorize?code_challenge=x");
                    Ok(Credentials {
                        plan: None,
                        account_id: Some("new-acct".into()),
                        email: Some("new@example.com".into()),
                        tokens: Tokens {
                            access: "fresh".into(),
                            refresh: Some("refresh".into()),
                            expires_at: Some(Timestamp::now() + SignedDuration::from_hours(1)),
                        },
                        source: CredentialSource::Subbier,
                    })
                })
            }
        }

        let harness = Harness::new("engine-login", PROXY_OFF);
        let poller = ScriptedPoller::with([vec![Ok(usage_at(3.0))]]);
        let (mut engine, handle) = harness
            .builder()
            .login(Arc::new(FakeLogin))
            .poller(poller)
            .build()
            .await
            .expect("build");

        let Engine { publisher, state } = &mut engine;
        state.begin_login(Provider::Claude);
        let url_event = state.events.recv().await.expect("a url event");
        state.handle_event(url_event, publisher).await;

        match handle.snapshot().login.clone() {
            Some(LoginState::AwaitingBrowser { provider, url, .. }) => {
                assert_eq!(provider, Provider::Claude);
                assert!(url.starts_with("https://auth.example.com/"));
            }
            other => panic!("expected AwaitingBrowser, got {other:?}"),
        }

        let done = state.events.recv().await.expect("a finish event");
        state.handle_event(done, publisher).await;
        let snap = handle.snapshot();
        assert!(snap.login.is_none(), "a finished login clears the state");
        assert_eq!(snap.subs.len(), 1);
        assert_eq!(
            creds::load_from(&harness.subs_path)
                .expect("subs.json")
                .len(),
            1,
            "a new sub is persisted to subbier's own store"
        );
    }

    #[tokio::test]
    async fn a_failed_login_surfaces_as_login_state_failed() {
        struct FailingLogin;
        impl LoginFlow for FailingLogin {
            fn login<'a>(&'a self, _provider: Provider, _on_url: UrlSink) -> LoginTask<'a> {
                Box::pin(async move { Err(Error::auth("the callback carried the wrong state")) })
            }
        }

        let harness = Harness::new("engine-login-fail", PROXY_OFF);
        let (mut engine, handle) = harness
            .builder()
            .login(Arc::new(FailingLogin))
            .build()
            .await
            .expect("build");

        let Engine { publisher, state } = &mut engine;
        state.begin_login(Provider::Codex);
        let event = state.events.recv().await.expect("a finish event");
        state.handle_event(event, publisher).await;

        match handle.snapshot().login.clone() {
            Some(LoginState::Failed { provider, error }) => {
                assert_eq!(provider, Provider::Codex);
                assert!(error.contains("wrong state"), "{error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(engine.state.subs.is_empty());
    }

    #[tokio::test]
    async fn disabling_a_sub_writes_it_through_and_stops_polling_it() {
        let harness = Harness::new("engine-disable", PROXY_OFF);
        let poller = ScriptedPoller::with([vec![Ok(usage_at(5.0)), Ok(usage_at(6.0))]]);
        creds::save_to(
            &harness.subs_path,
            &[
                sub(Provider::Codex, "acct-1", None),
                sub(Provider::Codex, "acct-2", None),
            ],
        )
        .expect("seed subs.json");

        let (mut engine, handle) = harness
            .builder()
            .poller(poller)
            .build()
            .await
            .expect("build");
        let publisher = &engine.publisher;

        engine
            .state
            .apply(
                Command::SetSubEnabled {
                    sub: SubId(2),
                    enabled: false,
                },
                publisher,
            )
            .await;

        let snap = handle.snapshot();
        assert!(snap.subs[0].enabled);
        assert!(!snap.subs[1].enabled);
        let text = harness.config_text();
        assert!(text.contains("sub \"codex:acct-2\""), "{text}");
        assert!(
            !Config::load_from(&harness.config_path)
                .expect("reparse")
                .sub_enabled(&SubKey::new(Provider::Codex, "acct-2")),
            "the toggle round-trips through the file"
        );

        engine.state.poll_round(false).await;
        assert!(engine.state.subs[0].usage.is_some());
        assert!(
            engine.state.subs[1].usage.is_none(),
            "a disabled sub is not polled"
        );
    }

    /// A watcher that reloaded every tick would drop `last_error` and re-sync the proxy every second.
    #[tokio::test]
    async fn a_hand_edit_reloads_itself_and_a_menu_click_does_not() {
        let harness = Harness::new("engine-config-watch", PROXY_OFF);
        let (mut engine, _handle) = harness.builder().build().await.expect("build");
        assert!(engine.state.config.pools.is_empty());
        assert!(
            !engine.state.reload_config_if_edited().await,
            "an untouched file is not an edit"
        );

        std::fs::write(
            &harness.config_path,
            "proxy {\n    enabled #false\n}\nui {\n    warn-pct 60\n}\n\
             pool \"moonshot\" {\n    max-sub-weekly-utilization 0.5\n}\n",
        )
        .expect("hand edit");

        assert!(engine.state.reload_config_if_edited().await);
        assert_eq!(engine.state.config.pools.len(), 1);
        assert_eq!(engine.state.config.pools[0].max_weekly_pct(), 50.0);
        assert_eq!(engine.state.build_snapshot().settings.warn_pct, 60.0);
        assert!(
            !engine.state.reload_config_if_edited().await,
            "the same file is only ever read once"
        );

        // The engine's own write moves the mtime too.
        let publisher = &engine.publisher;
        engine
            .state
            .apply(Command::SetMenuBar(MenuBarStyle::Icon), publisher)
            .await;
        assert!(!engine.state.reload_config_if_edited().await);
        assert_eq!(engine.state.config.pools.len(), 1, "the pool survived");
    }

    #[tokio::test]
    async fn removing_a_sub_forgets_it_everywhere() {
        let harness = Harness::new("engine-remove", PROXY_OFF);
        creds::save_to(
            &harness.subs_path,
            &[
                sub(Provider::Codex, "acct-1", None),
                sub(Provider::Codex, "acct-2", None),
            ],
        )
        .expect("seed subs.json");
        let (mut engine, handle) = harness.builder().build().await.expect("build");
        let publisher = &engine.publisher;

        engine.state.router.pin(Some(SubId(1)));
        engine
            .state
            .apply(Command::RemoveSub(SubId(1)), publisher)
            .await;

        assert_eq!(handle.snapshot().subs.len(), 1);
        assert_eq!(engine.state.registry.len(), 1);
        assert!(
            engine.state.router.pinned().is_none(),
            "the pin went with it"
        );
        assert_eq!(
            creds::load_from(&harness.subs_path)
                .expect("subs.json")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn the_loop_runs_and_shuts_down_on_command() {
        let harness = Harness::new("engine-loop", PROXY_OFF);
        let poller = ScriptedPoller::with([vec![Ok(usage_at(12.0))]]);
        creds::save_to(&harness.subs_path, &[sub(Provider::Codex, "acct-1", None)])
            .expect("seed subs.json");

        let (engine, handle) = harness
            .builder()
            .poller(poller.clone())
            .build()
            .await
            .expect("build");
        let task = tokio::spawn(engine.run());

        let mut snapshots = handle.subscribe();
        let percentage = loop {
            snapshots.changed().await.expect("the engine is alive");
            let snap = snapshots.borrow_and_update().clone();
            if let Some(window) = snap.subs.first().and_then(|s| s.session) {
                assert!(snap.generation >= 1, "generation 0 is the empty sentinel");
                break window.pct;
            }
        };
        assert_eq!(percentage, 12.0);

        handle.send(Command::Shutdown);
        task.await.expect("join").expect("clean shutdown");
        assert_eq!(poller.calls(), 1, "one round on the interval's first tick");
    }

    #[tokio::test]
    async fn a_read_only_frontend_never_binds_the_listener() {
        let harness = Harness::new(
            "engine-no-serve",
            "proxy {\n    enabled #true\n    bind \"127.0.0.1:0\"\n}\n",
        );
        let (mut engine, _handle) = harness
            .builder()
            .serve_proxy(false)
            .build()
            .await
            .expect("build");

        engine.state.sync_proxy().await;
        assert!(engine.state.proxy.is_none());

        let snap = engine.state.build_snapshot();
        assert!(!snap.proxy.running, "no listener");
        assert!(
            snap.settings.proxy_enabled,
            "the user's setting still reads back as they wrote it"
        );
    }

    #[tokio::test]
    async fn rotated_tokens_are_debounced_into_subbiers_own_store() {
        let harness = Harness::new("engine-tokens", PROXY_OFF);
        creds::save_to(&harness.subs_path, &[sub(Provider::Codex, "acct-1", None)])
            .expect("seed subs.json");
        let (mut engine, _handle) = harness.builder().build().await.expect("build");
        let Engine { publisher, state } = &mut engine;

        let mut rotated = sub(Provider::Codex, "acct-1", None);
        rotated.credentials.tokens.access = "rotated".into();
        for _ in 0..5 {
            state
                .handle_event(Event::TokensRefreshed(Box::new(rotated.clone())), publisher)
                .await;
        }

        assert!(state.persist_at.is_some(), "one write is armed");
        assert_eq!(
            creds::load_from(&harness.subs_path).expect("subs.json")[0]
                .credentials
                .tokens
                .access,
            "access-token",
            "five rotations, no file writes yet"
        );

        // What the loop's persist arm does when the timer fires.
        state.persist_subs();
        assert_eq!(
            creds::load_from(&harness.subs_path).expect("subs.json")[0]
                .credentials
                .tokens
                .access,
            "rotated",
            "subbier keeps its own copy; the vendor's file is never written"
        );
        assert!(state.persist_at.is_none());
    }

    #[test]
    fn bind_resolution_covers_the_shapes_a_config_can_hold() {
        assert_eq!(
            resolve_bind("127.0.0.1:8787").unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787)
        );
        assert_eq!(
            resolve_bind("localhost:9000").unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9000)
        );
        assert!(resolve_bind("[::1]:8787").is_ok());
        assert!(resolve_bind("nonsense").is_err());
        assert!(resolve_bind("127.0.0.1:not-a-port").is_err());
    }

    #[test]
    fn a_poll_interval_is_never_zero() {
        assert_eq!(poll_period(SignedDuration::ZERO), MIN_POLL_INTERVAL);
        assert_eq!(
            poll_period(SignedDuration::from_secs(-5)),
            Duration::from_secs(180),
            "a negative interval is nonsense, not a fast poll"
        );
        assert_eq!(
            poll_period(SignedDuration::from_secs(300)),
            Duration::from_secs(300)
        );
    }
}
