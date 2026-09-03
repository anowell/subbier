//! The `Snapshot` / `Command` boundary every frontend sees. A [`Snapshot`] is
//! immutable and published on a [`tokio::sync::watch`]; a [`Command`] never
//! returns a value. Its JSON is the `subbier status --json` document: a
//! [`Timestamp`] is RFC 3339 and a [`SignedDuration`] an ISO-8601 string.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::sync::{mpsc, watch};

use crate::model::{
    CredentialSource, MenuBarStyle, Projection, Provider, Severity, StrategyKind, SubId, SubKey,
    WindowKind,
};

/// An immutable view of everything the engine knows, at one instant.
#[derive(Debug, Clone)]
pub struct Snapshot(Arc<SnapshotData>);

impl Snapshot {
    #[must_use]
    pub fn empty() -> Self {
        Self(Arc::new(SnapshotData::default()))
    }

    /// `true` while nothing has ever been published (`generation == 0`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.generation == 0
    }
}

impl Deref for Snapshot {
    type Target = SnapshotData;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<SnapshotData> for Snapshot {
    fn as_ref(&self) -> &SnapshotData {
        &self.0
    }
}

impl From<SnapshotData> for Snapshot {
    fn from(data: SnapshotData) -> Self {
        Self(Arc::new(data))
    }
}

impl Default for Snapshot {
    fn default() -> Self {
        Self::empty()
    }
}

impl Serialize for Snapshot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Snapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        SnapshotData::deserialize(deserializer).map(Snapshot::from)
    }
}

/// The payload behind a [`Snapshot`]. Never constructed by a frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotData {
    /// Monotonic; `0` means nothing has been published yet, first publish is `1`.
    pub generation: u64,
    pub captured_at: Timestamp,
    pub subs: Vec<SubView>,
    /// In `config.kdl` order — the order a frontend draws their tabs.
    pub pools: Vec<PoolView>,
    /// Allowance-weighted mean of each enabled account's worst window. `None`
    /// (never `0`) when unknown.
    pub overall_pct: Option<f32>,
    pub proxy: ProxyView,
    pub settings: SettingsView,
    /// Highest severity across all enabled subs.
    pub worst: Severity,
    /// Band crossings since the previous snapshot; idempotent to drop.
    pub alerts: Vec<Alert>,
    pub login: Option<LoginState>,
    pub last_error: Option<String>,
}

impl Default for SnapshotData {
    fn default() -> Self {
        Self {
            generation: 0,
            captured_at: Timestamp::now(),
            subs: Vec::new(),
            proxy: ProxyView::default(),
            settings: SettingsView::default(),
            worst: Severity::Ok,
            alerts: Vec::new(),
            login: None,
            last_error: None,
            pools: Vec::new(),
            overall_pct: None,
        }
    }
}

/// One `pool` block — a named subset of the accounts with its own base URL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolView {
    /// The pool name, as written. Also its URL path segment.
    pub name: String,
    /// Restricted to one provider, or `None` for both.
    pub provider: Option<Provider>,
    /// Every member in [`SubId`] order, including ones held back by a ceiling.
    pub members: Vec<SubId>,
    /// The members the router would actually route to right now.
    pub eligible: Vec<SubId>,
    /// Session ceiling as a percentage. `100.0` means none set.
    pub max_session_pct: f32,
    /// Weekly ceiling as a percentage. `100.0` means none set.
    pub max_weekly_pct: f32,
    /// Ready-to-paste; `None` while the proxy is down.
    pub openai_base_url: Option<String>,
    /// Ready-to-paste; `None` while the proxy is down.
    pub anthropic_base_url: Option<String>,
    /// Proxy-observed on this pool's own endpoint, not summed over its members
    /// (a member is reachable on the bare proxy and in other pools too).
    pub proxied_in_flight: u32,
    /// Proxy-observed on this pool's own endpoint, in the last hour.
    pub proxied_tokens_1h: u64,
}

/// One subscription, as a frontend sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubView {
    pub id: SubId,
    /// The persisted identity keying the sqlite history; [`SubId`] is
    /// per-process. `serde(default)` because `status` and `watch` parse this
    /// document off a possibly older instance — as every field added here must.
    #[serde(default)]
    pub key: SubKey,
    pub provider: Provider,
    /// Already disambiguated between same-looking accounts. Render verbatim;
    /// never re-derive a label.
    pub label: String,
    /// `"plus"`, `"max20"`, `"team"`, … exactly as the provider spelled it.
    pub plan: Option<String>,
    /// The resolved tier's canonical id — `"max-5x"`, `"plus"`, `"unknown"`.
    pub plan_tier: String,
    /// Allowance weight used, after any `plan-weights` override —
    /// `overall_pct`'s working.
    pub plan_weight: f32,
    pub source: CredentialSource,
    pub enabled: bool,
    pub health: SubHealth,
    pub session: Option<WindowView>,
    pub weekly: Option<WindowView>,
    /// Narrower limits, e.g. weekly Fable-only.
    pub scoped: Vec<ScopedWindow>,
    /// Proxy-observed counters. Read [`RoutingView`] before rendering.
    pub routing: RoutingView,
}

/// A named narrower allowance window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopedWindow {
    /// The provider's own name for the limit, e.g. `"fable"`.
    pub name: String,
    pub window: WindowView,
}

/// One allowance window, straight from the provider's usage API — never
/// computed, smoothed or backfilled from proxy token counts.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowView {
    /// Percent of allowance consumed, `0..=100`.
    pub pct: f32,
    pub resets_at: Option<Timestamp>,
    /// Precomputed so a frontend needs no date library.
    pub resets_in: Option<SignedDuration>,
    pub severity: Severity,
    /// Only `Some` when the projection lands before `resets_at`; one past the
    /// reset is not actionable and is withheld.
    pub projection: Option<Projection>,
}

/// Why a sub's numbers might not be trustworthy, or why it is being skipped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubHealth {
    Ok,
    /// Last poll failed; the numbers shown are the previous ones.
    Stale {
        since: Timestamp,
        error: String,
    },
    /// Never successfully polled.
    Unknown,
    /// Confirmed at 100% or refused by upstream. Skipped until `until`.
    Exhausted {
        until: Timestamp,
    },
    /// The refresh token is dead. Needs a re-login; skipped indefinitely.
    NeedsLogin {
        error: String,
    },
}

/// Proxy-observed attribution for one sub.
/// Every `proxied_*` counter here measures only traffic subbier routed, while
/// allowance percentages ([`WindowView`]) are account-wide: the two series may
/// diverge arbitrarily, so never plot them on one axis.
/// measures only traffic subbier routed. Allowance percentages ([`WindowView`])
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct RoutingView {
    pub eligible: bool,
    /// Is this the sub the proxy is currently pinned/sticky to for its provider?
    pub active: bool,
    /// Since the engine started.
    pub proxied_requests_total: u64,
    pub last_proxied_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyView {
    pub running: bool,
    pub configured_bind: SocketAddr,
    /// What we actually bound. `None` while the proxy is down.
    pub listening: Option<SocketAddr>,
    /// Ready-to-paste; `None` while the proxy is down.
    pub openai_base_url: Option<String>,
    /// Ready-to-paste; `None` while the proxy is down.
    pub anthropic_base_url: Option<String>,
    /// Whether clients must present `proxy.key`.
    pub requires_key: bool,
    /// Totals across every endpoint; see [`RoutingView`].
    pub proxied_in_flight: u32,
    pub proxied_requests_total: u64,
    pub proxied_tokens_1h: u64,
    pub last_error: Option<String>,

    /// The process that built this snapshot, hence over `GET /status` the one
    /// owning the listener: `subbier service status` compares it with launchd's
    /// pid. `None` only from an instance too old to send it.
    #[serde(default)]
    pub pid: Option<u32>,
    /// That process's subbier version — a running daemon and the CLI asking it
    /// can be different builds.
    #[serde(default)]
    pub version: Option<String>,
}

impl Default for ProxyView {
    fn default() -> Self {
        Self {
            running: false,
            // Mirrors the `proxy.bind` default in config.kdl.
            configured_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787),
            listening: None,
            openai_base_url: None,
            anthropic_base_url: None,
            requires_key: false,
            proxied_in_flight: 0,
            proxied_requests_total: 0,
            proxied_tokens_1h: 0,
            last_error: None,
            pid: None,
            version: None,
        }
    }
}

/// Effective settings: one `config.kdl` key, one menu control and one
/// [`Command`] per field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsView {
    pub proxy_enabled: bool,
    pub auto_switch: bool,
    pub strategy: StrategyKind,
    /// Effective value: `proxy.sticky` if set, else the strategy's default.
    pub sticky: bool,
    /// Indexed by [`Provider::index`]; iterate with [`Provider::ALL`].
    pub providers_proxied: [bool; 2],
    pub poll_interval: SignedDuration,
    pub warn_pct: f32,
    pub critical_pct: f32,
    pub notifications_enabled: bool,
    pub menu_bar: MenuBarStyle,
    pub launch_at_login: bool,
}

impl Default for SettingsView {
    fn default() -> Self {
        let config = crate::config::Config::default();
        Self {
            proxy_enabled: config.proxy.enabled,
            auto_switch: config.proxy.auto_switch,
            strategy: config.proxy.strategy,
            sticky: config.proxy.effective_sticky(),
            providers_proxied: [config.proxy.codex, config.proxy.claude],
            poll_interval: config.poll.interval,
            warn_pct: config.ui.warn_pct,
            critical_pct: config.ui.critical_pct,
            notifications_enabled: config.ui.notifications,
            menu_bar: config.ui.menu_bar,
            launch_at_login: config.ui.launch_at_login,
        }
    }
}

impl SettingsView {
    #[must_use]
    pub const fn proxies(&self, provider: Provider) -> bool {
        self.providers_proxied[provider.index()]
    }
}

/// An OAuth login the engine is in the middle of, or has just failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LoginState {
    /// The browser has been opened; we are waiting on the loopback callback.
    AwaitingBrowser {
        provider: Provider,
        url: String,
        started_at: Timestamp,
    },
    /// The last attempt failed. Cleared by the next [`Command::Login`].
    Failed { provider: Provider, error: String },
}

/// A severity band crossing observed since the previous snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    pub sub: SubId,
    pub window: WindowKind,
    /// Only `Warn` or `Critical` are ever emitted.
    pub severity: Severity,
    pub pct: f32,
}

/// Everything a frontend can ask the engine to do.
///
/// Commands never return anything: the outcome, failure included, appears in
/// the next [`Snapshot`]'s `last_error`, `login`, `health` or settings.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Command {
    SetProxyEnabled(bool),
    SetAutoSwitch(bool),
    SetStrategy(StrategyKind),
    /// `None` means "use the strategy's default stickiness".
    SetSticky(Option<bool>),
    SetProviderProxied {
        provider: Provider,
        on: bool,
    },
    SetSubEnabled {
        sub: SubId,
        enabled: bool,
    },
    SetNotificationsEnabled(bool),
    SetMenuBar(MenuBarStyle),
    SetLaunchAtLogin(bool),

    /// Force this sub for every subsequent request. `None` clears the pin.
    Pin(Option<SubId>),
    /// Re-poll usage. `force` bypasses the cache.
    RefreshUsage {
        force: bool,
    },
    ClearExhaustion(SubId),
    Rediscover,
    Login(Provider),
    CancelLogin,
    RemoveSub(SubId),
    ReloadConfig,
    Shutdown,
}

/// The engine side of the boundary. A frontend only ever holds a [`Handle`].
#[derive(Debug)]
pub struct Publisher {
    tx: watch::Sender<Snapshot>,
    commands: mpsc::UnboundedReceiver<Command>,
    generation: AtomicU64,
}

impl Publisher {
    /// The `watch` starts out holding [`Snapshot::empty`], so
    /// [`Handle::snapshot`] never blocks.
    #[must_use]
    pub fn new() -> (Publisher, Handle) {
        let (tx, rx) = watch::channel(Snapshot::empty());
        let (cmd_tx, commands) = mpsc::unbounded_channel();
        let publisher = Publisher {
            tx,
            commands,
            generation: AtomicU64::new(0),
        };
        let handle = Handle {
            commands: cmd_tx,
            snapshots: rx,
        };
        (publisher, handle)
    }

    /// Overwrites `data`'s `generation` and `captured_at`; the publisher is the
    /// only thing allowed to set them.
    pub fn publish(&self, mut data: SnapshotData) {
        data.generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        data.captured_at = Timestamp::now();
        self.tx.send_replace(Snapshot::from(data));
    }

    pub fn commands(&mut self) -> &mut mpsc::UnboundedReceiver<Command> {
        &mut self.commands
    }
}

/// A frontend's entire view of the engine. Cheap to clone; hand one to every
/// thread, task or menu callback that needs it.
#[derive(Debug, Clone)]
pub struct Handle {
    commands: mpsc::UnboundedSender<Command>,
    snapshots: watch::Receiver<Snapshot>,
}

impl Handle {
    /// Fire and forget: a command dropped because the engine is gone is logged
    /// and swallowed.
    pub fn send(&self, cmd: Command) {
        if self.commands.send(cmd).is_err() {
            tracing::debug!("command dropped: the engine is gone");
        }
    }

    /// The latest snapshot. Never blocks; never fails.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.snapshots.borrow().clone()
    }

    /// A receiver for change-driven redraw loops.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.snapshots.clone()
    }
}

// These hop between tokio workers and the AppKit main thread; nothing !Send may
// exist outside `subbier-macos`.
const _: fn() = || {
    fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    fn assert_send_static<T: Send + 'static>() {}
    assert_send_sync_static::<Snapshot>();
    assert_send_sync_static::<Handle>();
    assert_send_sync_static::<Publisher>();
    assert_send_static::<Command>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloning_a_snapshot_shares_its_payload_rather_than_copying_it() {
        let a = Snapshot::empty();
        let b = a.clone();
        assert!(std::ptr::eq(
            &*a as *const SnapshotData,
            &*b as *const SnapshotData
        ));
    }

    fn sample_sub() -> SubView {
        SubView {
            id: SubId(1),
            key: SubKey::new(Provider::Codex, "acct-1"),
            provider: Provider::Codex,
            label: "work".into(),
            plan: Some("plus".into()),
            plan_tier: "plus".into(),
            plan_weight: 1.0,
            source: CredentialSource::Keychain,
            enabled: true,
            health: SubHealth::Ok,
            session: Some(WindowView {
                pct: 42.0,
                resets_at: Some(Timestamp::UNIX_EPOCH),
                resets_in: Some(SignedDuration::from_secs(60)),
                severity: Severity::Ok,
                projection: None,
            }),
            weekly: None,
            scoped: vec![ScopedWindow {
                name: "fable".into(),
                window: WindowView {
                    pct: 3.0,
                    resets_at: None,
                    resets_in: None,
                    severity: Severity::Ok,
                    projection: None,
                },
            }],
            routing: RoutingView {
                eligible: true,
                active: true,
                proxied_requests_total: 17,
                last_proxied_at: Some(Timestamp::UNIX_EPOCH),
            },
        }
    }

    /// A bare `tokens`/`connections` on a sub row invites reading a proxy metric as an account one.
    #[test]
    fn the_json_document_names_every_field_it_promises() {
        let json = serde_json::to_string(&Snapshot::empty()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        for field in [
            "generation",
            "captured_at",
            "subs",
            "pools",
            "overall_pct",
            "proxy",
            "settings",
            "worst",
            "alerts",
            "login",
            "last_error",
        ] {
            assert!(value.get(field).is_some(), "missing {field} in {value}");
        }
        for field in [
            "proxy_enabled",
            "auto_switch",
            "strategy",
            "sticky",
            "providers_proxied",
            "poll_interval",
            "warn_pct",
            "critical_pct",
            "notifications_enabled",
            "menu_bar",
            "launch_at_login",
        ] {
            assert!(value["settings"].get(field).is_some(), "missing {field}");
        }
        // jiff serialises a SignedDuration as an ISO-8601 duration string.
        assert_eq!(value["settings"]["poll_interval"], "PT3M");

        let routing = serde_json::to_value(sample_sub().routing).unwrap();
        for banned in [
            "tokens",
            "tokens_last_hour",
            "connections",
            "in_flight",
            "requests_total",
            "proxied_in_flight",
            "proxied_tokens_1h",
        ] {
            assert!(routing.get(banned).is_none(), "unprovenanced {banned}");
        }
        let proxy = serde_json::to_value(ProxyView::default()).unwrap();
        for field in [
            "proxied_in_flight",
            "proxied_requests_total",
            "proxied_tokens_1h",
        ] {
            assert!(proxy.get(field).is_some(), "missing {field}");
        }
    }

    #[test]
    fn a_populated_snapshot_round_trips_through_json() {
        let data = SnapshotData {
            subs: vec![sample_sub()],
            worst: Severity::Warn,
            alerts: vec![Alert {
                sub: SubId(1),
                window: WindowKind::Scoped("fable".into()),
                severity: Severity::Warn,
                pct: 76.5,
            }],
            login: Some(LoginState::AwaitingBrowser {
                provider: Provider::Codex,
                url: "https://example.invalid/authorize".into(),
                started_at: Timestamp::UNIX_EPOCH,
            }),
            last_error: Some("nope".into()),
            ..SnapshotData::default()
        };
        let snap = Snapshot::from(data);
        let json = serde_json::to_string(&snap).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(back.subs, snap.subs);
        assert_eq!(back.alerts, snap.alerts);
        assert_eq!(back.login, snap.login);

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["subs"][0]["health"]["state"], "ok");
        assert_eq!(value["login"]["state"], "awaiting_browser");
        // Matches the sqlite `allowance_sample.window` column.
        assert_eq!(value["alerts"][0]["window"], "fable");
    }

    #[tokio::test]
    async fn publish_stamps_a_monotonic_generation_and_a_capture_time() {
        let (publisher, handle) = Publisher::new();
        assert_eq!(handle.snapshot().generation, 0);
        let mut rx = handle.subscribe();

        let before = Timestamp::now();
        // Deliberately lie about generation: the publisher owns it.
        publisher.publish(SnapshotData {
            generation: 999,
            ..SnapshotData::default()
        });
        let first = handle.snapshot();
        assert_eq!(first.generation, 1);
        assert!(first.captured_at >= before);
        assert!(!first.is_empty());

        // A burst collapses into one wakeup at the newest value.
        for _ in 0..2 {
            publisher.publish(SnapshotData::default());
        }
        rx.changed().await.unwrap();
        assert_eq!(rx.borrow_and_update().generation, 3);
    }

    #[tokio::test]
    async fn commands_reach_the_engine_in_order() {
        let (mut publisher, handle) = Publisher::new();
        handle.send(Command::SetProxyEnabled(false));
        handle.send(Command::Shutdown);

        let rx = publisher.commands();
        assert_eq!(rx.recv().await, Some(Command::SetProxyEnabled(false)));
        assert_eq!(rx.recv().await, Some(Command::Shutdown));
    }

    #[tokio::test]
    async fn a_dead_engine_or_a_dead_frontend_is_not_an_error() {
        let (publisher, handle) = Publisher::new();
        drop(publisher);
        handle.send(Command::Rediscover); // must not panic
        assert_eq!(handle.snapshot().generation, 0);

        let (publisher, handle) = Publisher::new();
        drop(handle);
        publisher.publish(SnapshotData::default());
    }
}
