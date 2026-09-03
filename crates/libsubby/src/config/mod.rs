//! `~/.subbier/config.kdl`: user intent only — anything recomputable at startup
//! (the discovered subs, usage, exhaustion) is deliberately absent. Reading goes
//! through serde; writing edits the parsed [`kdl::KdlDocument`] in place so a
//! menu toggle cannot eat a user's comments (see [`write::set`]).

pub mod write;

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use jiff::SignedDuration;
use kdl::KdlDocument;
use serde::de::value::{MapAccessDeserializer, SeqAccessDeserializer};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::error::{Error, Result};
use crate::model::{MenuBarStyle, Provider, StrategyKind, SubKey};
use crate::severity::{DEFAULT_CRITICAL_PCT, DEFAULT_WARN_PCT};

/// The default listen address: loopback, so no `proxy.key` is required.
pub const DEFAULT_BIND: &str = "127.0.0.1:8787";

/// The default `poll.interval`: 1% of Claude's 5h session window, the finest
/// thing subbier draws. Faster cannot move the rendered number and only earns
/// 429s; slower would rank accounts for the router off stale figures.
pub const DEFAULT_POLL_INTERVAL: SignedDuration = SignedDuration::from_secs(180);

/// The whole of `config.kdl`, with every default already applied. Never
/// serialised back — that would discard the user's comments; see [`write::set`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub poll: PollConfig,
    pub ui: UiConfig,
    pub history: HistoryConfig,
    /// Per-sub overrides from the repeated top-level `sub "<key>" { … }` nodes,
    /// keyed by the stable [`SubKey`], never by [`crate::SubId`].
    pub subs: BTreeMap<SubKey, SubOverride>,
    /// Named pools from the repeated top-level `pool "<name>" { … }` nodes,
    /// **in file order** — that is the order the menu draws their tabs.
    pub pools: Vec<PoolConfig>,
    /// `plan-weights { "codex:pro" 6 }` — per-tier overrides of
    /// [`crate::plan::PlanTier`]'s weights, keyed by `provider:id`.
    pub plan_weights: BTreeMap<String, f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProxyConfig {
    pub enabled: bool,
    /// `host:port` to bind. Off-loopback binds require [`ProxyConfig::key`].
    pub bind: String,
    /// Shared secret required from clients. `None` is only safe on loopback.
    pub key: Option<String>,
    pub strategy: StrategyKind,
    /// Whether the router may move off an exhausted sub on its own.
    pub auto_switch: bool,
    /// `None` means this strategy's default; stickiness is a separate axis from
    /// ranking, not a fifth strategy.
    pub sticky: Option<bool>,
    /// Front the Codex Responses API.
    pub codex: bool,
    /// Front the Anthropic Messages API.
    pub claude: bool,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: DEFAULT_BIND.to_string(),
            key: None,
            strategy: StrategyKind::RoundRobin,
            auto_switch: true,
            sticky: None,
            codex: true,
            claude: true,
        }
    }
}

impl ProxyConfig {
    #[must_use]
    pub fn effective_sticky(&self) -> bool {
        self.sticky
            .unwrap_or_else(|| self.strategy.default_sticky())
    }

    /// A bare hostname other than `localhost` counts as **not** loopback: we
    /// will not resolve DNS to decide whether a config needs a key.
    fn bind_is_loopback(&self) -> Result<bool> {
        let (host, port) = self.bind.rsplit_once(':').ok_or_else(|| {
            Error::config(format!(
                "proxy.bind is {:?}, which is not \"host:port\"",
                self.bind
            ))
        })?;
        port.parse::<u16>().map_err(|_| {
            Error::config(format!(
                "proxy.bind is {:?}, whose port {port:?} is not a number",
                self.bind
            ))
        })?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.eq_ignore_ascii_case("localhost") {
            return Ok(true);
        }
        Ok(host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PollConfig {
    pub interval: SignedDuration,
    /// Deadline for one usage-scoring round, shared across every sub in it —
    /// not a per-request timeout.
    pub usage_timeout: SignedDuration,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            interval: DEFAULT_POLL_INTERVAL,
            usage_timeout: SignedDuration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UiConfig {
    pub warn_pct: f32,
    pub critical_pct: f32,
    /// Whether threshold crossings raise a desktop notification.
    pub notifications: bool,
    pub menu_bar: MenuBarStyle,
    pub launch_at_login: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            warn_pct: DEFAULT_WARN_PCT,
            critical_pct: DEFAULT_CRITICAL_PCT,
            notifications: false,
            menu_bar: MenuBarStyle::IconPercent,
            launch_at_login: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryConfig {
    /// Days of `allowance_sample` / `proxied_request` rows to retain.
    pub retain_days: u32,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self { retain_days: 7 }
    }
}

/// A per-sub override from a `sub "<key>" { … }` node.
#[derive(Debug, Clone, PartialEq)]
pub struct SubOverride {
    /// Whether this sub may be routed to. Absent means enabled.
    pub enabled: bool,
    /// Replaces the discovered label.
    pub label: Option<String>,
}

impl Default for SubOverride {
    fn default() -> Self {
        Self {
            enabled: true,
            label: None,
        }
    }
}

/// One `pool "<name>" { … }` node: a named subset of the accounts, reachable on
/// its own base URL at `/pool/<name>`. Membership rather than budget, so the
/// accounts a pool was not given stay untouched whatever it does.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolConfig {
    /// Path segment and display name. Unique, non-empty, no `/`.
    pub name: String,
    /// Restrict the pool to one provider. `None` serves both.
    pub provider: Option<Provider>,
    /// Members, written as emails, labels or full sub keys — see
    /// [`PoolConfig::matches`]. **Empty means every sub**, which is what makes a
    /// pool that only sets a ceiling useful.
    pub subs: Vec<PoolMember>,
    /// Skip a member whose **session** allowance is at or above this fraction
    /// (`0.0..=1.0`). `1.0`, the default, never skips.
    pub max_sub_session_utilization: f32,
    /// Skip a member whose **weekly** allowance is at or above this fraction.
    pub max_sub_weekly_utilization: f32,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            provider: None,
            subs: Vec::new(),
            max_sub_session_utilization: 1.0,
            max_sub_weekly_utilization: 1.0,
        }
    }
}

/// One account a pool admits. The per-member `provider` narrows that member
/// alone — one email is usually two subscriptions — and both must admit a sub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolMember {
    /// Which subscription of this account, or `None` for any.
    pub provider: Option<Provider>,
    /// An email, a label, a full sub key, or a bare account id.
    pub id: String,
}

impl PoolMember {
    /// A member named without a provider — what a `subs "…"` entry is.
    #[must_use]
    pub fn any(id: impl Into<String>) -> Self {
        Self {
            provider: None,
            id: id.into(),
        }
    }

    #[must_use]
    pub fn matches(&self, key: &SubKey, label: &str, email: Option<&str>) -> bool {
        if self.provider.is_some_and(|p| key.provider() != Some(p)) {
            return false;
        }
        let want = self.id.trim();
        want.eq_ignore_ascii_case(key.as_str())
            || want.eq_ignore_ascii_case(label)
            || email.is_some_and(|e| want.eq_ignore_ascii_case(e))
            // a bare account id: the `provider:` prefix may be left off
            || want.eq_ignore_ascii_case(key.account())
    }
}

impl PoolConfig {
    /// Whether this pool admits a sub. Members match case-insensitively on
    /// email, label, sub key or bare account id, and an **empty** member list
    /// admits every sub the pool-wide `provider` filter allows.
    #[must_use]
    pub fn matches(&self, key: &SubKey, label: &str, email: Option<&str>) -> bool {
        if self.provider.is_some_and(|p| key.provider() != Some(p)) {
            return false;
        }
        if self.subs.is_empty() {
            return true;
        }
        self.subs
            .iter()
            .any(|member| member.matches(key, label, email))
    }

    /// The session ceiling as a percentage (`0..=100`), as every `pct` here is.
    #[must_use]
    pub fn max_session_pct(&self) -> f32 {
        self.max_sub_session_utilization * 100.0
    }

    /// The weekly ceiling as a percentage (`0..=100`).
    #[must_use]
    pub fn max_weekly_pct(&self) -> f32 {
        self.max_sub_weekly_utilization * 100.0
    }
}

impl Config {
    /// `$SUBBIER_HOME/config.kdl`, else `~/.subbier/config.kdl`.
    #[must_use]
    pub fn path() -> PathBuf {
        crate::store::home().join("config.kdl")
    }

    pub fn load() -> Result<Config> {
        Self::load_from(&Self::path())
    }

    /// A missing file is `Ok(Config::default())` — an absent config is a fully
    /// working install — but one that exists and does not parse is an error.
    pub fn load_from(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn parse(text: &str) -> Result<Config> {
        let mut doc: KdlDocument = text.parse()?;
        for warning in retired_keys(&doc) {
            tracing::warn!("{warning}");
        }
        fill_in_bare_flags(&mut doc);
        let raw: RawConfig = kdl::de::from_str(&doc.to_string())?;
        let config = raw.resolve()?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if !self.proxy.bind_is_loopback()? && self.proxy.key.is_none() {
            return Err(Error::config(format!(
                "proxy.bind is {:?}, which is not loopback, but proxy.key is unset: \
                 set proxy.key to a shared secret, or bind proxy.bind to 127.0.0.1",
                self.proxy.bind
            )));
        }
        let mut seen: Vec<&str> = Vec::new();
        for pool in &self.pools {
            let name = pool.name.trim();
            if name.is_empty() {
                return Err(Error::config(
                    "a pool has an empty name: write `pool \"moonshot\" { … }`".to_string(),
                ));
            }
            // the name is a URL path segment, so nothing that re-splits a path
            if let Some(bad) = name
                .chars()
                .find(|c| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '%'))
            {
                return Err(Error::config(format!(
                    "pool {name:?} contains {bad:?}, which cannot appear in the \
                     `/pool/<name>` URL it is served on",
                )));
            }
            // unique as the path is matched: one route cannot have two configs
            if seen.iter().any(|s| s.eq_ignore_ascii_case(name)) {
                return Err(Error::config(format!(
                    "pool {name:?} is defined more than once",
                )));
            }
            seen.push(name);
            for (key, value) in [
                (
                    "max-sub-session-utilization",
                    pool.max_sub_session_utilization,
                ),
                (
                    "max-sub-weekly-utilization",
                    pool.max_sub_weekly_utilization,
                ),
            ] {
                if !(0.0..=1.0).contains(&value) || value.is_nan() {
                    return Err(Error::config(format!(
                        "pool {name:?} sets {key} to {value}, but it is a fraction of the \
                         allowance: write 0.5 for half, not 50",
                    )));
                }
            }
        }
        Ok(())
    }

    /// The pool of this name, matched case-insensitively as the URL is.
    #[must_use]
    pub fn pool(&self, name: &str) -> Option<&PoolConfig> {
        self.pools
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name.trim()))
    }

    /// Whether a sub is enabled. Unmentioned subs are enabled.
    #[must_use]
    pub fn sub_enabled(&self, key: &SubKey) -> bool {
        self.subs.get(key).is_none_or(|s| s.enabled)
    }

    #[must_use]
    pub fn sub_label(&self, key: &SubKey) -> Option<&str> {
        self.subs.get(key).and_then(|s| s.label.as_deref())
    }
}

// Every field is an `Option` and every default lands in `resolve()`: kdl-serde
// hands a *missing* child node to the deserializer as `false`, so a plain `bool`
// could not tell "wrote nothing" from "wrote `#false`".

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct RawConfig {
    proxy: RawProxy,
    poll: RawPoll,
    ui: RawUi,
    history: RawHistory,
    #[serde(rename = "sub", deserialize_with = "deserialize_subs")]
    subs: BTreeMap<SubKey, SubOverride>,
    #[serde(rename = "pool", deserialize_with = "deserialize_pools")]
    pools: Vec<PoolConfig>,
    plan_weights: BTreeMap<String, f32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct RawProxy {
    enabled: Option<bool>,
    bind: Option<String>,
    key: Option<String>,
    strategy: Option<StrategyKind>,
    auto_switch: Option<bool>,
    sticky: Option<bool>,
    codex: Option<bool>,
    claude: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct RawPoll {
    interval: Option<String>,
    usage_timeout: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct RawUi {
    warn_pct: Option<f32>,
    critical_pct: Option<f32>,
    notifications: Option<bool>,
    menu_bar: Option<MenuBarStyle>,
    launch_at_login: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct RawHistory {
    retain_days: Option<u32>,
}

impl RawConfig {
    fn resolve(self) -> Result<Config> {
        let defaults = Config::default();
        Ok(Config {
            proxy: ProxyConfig {
                enabled: self.proxy.enabled.unwrap_or(defaults.proxy.enabled),
                bind: self.proxy.bind.unwrap_or(defaults.proxy.bind),
                key: self.proxy.key,
                strategy: self.proxy.strategy.unwrap_or(defaults.proxy.strategy),
                auto_switch: self.proxy.auto_switch.unwrap_or(defaults.proxy.auto_switch),
                sticky: self.proxy.sticky,
                codex: self.proxy.codex.unwrap_or(defaults.proxy.codex),
                claude: self.proxy.claude.unwrap_or(defaults.proxy.claude),
            },
            poll: PollConfig {
                interval: duration("poll.interval", self.poll.interval)?
                    .unwrap_or(defaults.poll.interval),
                usage_timeout: duration("poll.usage-timeout", self.poll.usage_timeout)?
                    .unwrap_or(defaults.poll.usage_timeout),
            },
            ui: UiConfig {
                warn_pct: self.ui.warn_pct.unwrap_or(defaults.ui.warn_pct),
                critical_pct: self.ui.critical_pct.unwrap_or(defaults.ui.critical_pct),
                notifications: self.ui.notifications.unwrap_or(defaults.ui.notifications),
                menu_bar: self.ui.menu_bar.unwrap_or(defaults.ui.menu_bar),
                launch_at_login: self
                    .ui
                    .launch_at_login
                    .unwrap_or(defaults.ui.launch_at_login),
            },
            history: HistoryConfig {
                retain_days: self
                    .history
                    .retain_days
                    .unwrap_or(defaults.history.retain_days),
            },
            subs: self.subs,
            pools: self.pools,
            plan_weights: self.plan_weights,
        })
    }
}

/// Durations are written the way a human writes them: `"60s"`, `"1h 30m"`.
fn duration(key: &str, text: Option<String>) -> Result<Option<SignedDuration>> {
    text.map(|text| {
        text.parse::<SignedDuration>()
            .map_err(|e| Error::config(format!("{key} is {text:?}, which is not a duration: {e}")))
    })
    .transpose()
}

/// Keys an older subbier read and this one ignores, as `(block, key, advice)`.
/// Unknown keys are never a parse error, but one that used to *do* something
/// earns a warning rather than leaving the user staring at a dead setting.
const RETIRED_KEYS: &[(&str, &str, &str)] = &[(
    "ui",
    "default-tab",
    "tabs select a pool now, not an agent, so none of its values (auto / codex / \
         claude / settings) names a tab any more; the menu opens on `All subs`",
)];

/// One message per [`RETIRED_KEYS`] entry present in `doc`.
fn retired_keys(doc: &KdlDocument) -> Vec<String> {
    RETIRED_KEYS
        .iter()
        .filter(|(block, key, _)| {
            doc.get(block)
                .and_then(|node| node.children())
                .is_some_and(|children| children.get(key).is_some())
        })
        .map(|(block, key, advice)| {
            format!("{block}.{key} in config.kdl is no longer read: {advice}")
        })
        .collect()
}

/// Give every childless, argument-less node inside a block an explicit `#true`:
/// KDL reads a bare `notifications` as `notifications #true`, but kdl-serde only
/// honours that for plain `bool`, and ours are `Option<bool>`.
fn fill_in_bare_flags(doc: &mut KdlDocument) {
    for node in doc.nodes_mut() {
        let Some(children) = node.children_mut().as_mut() else {
            continue;
        };
        for child in children.nodes_mut() {
            if child.entries().is_empty() && child.children().is_none() {
                child.push(kdl::KdlEntry::new(true));
            }
        }
    }
}

/// One `sub "<key>" { … }` node. `#0` is kdl-serde for the first argument.
#[derive(Debug, Deserialize)]
struct SubEntry {
    #[serde(rename = "#0")]
    key: SubKey,
    enabled: Option<bool>,
    label: Option<String>,
}

const SUB_FIELDS: &[&str] = &["#0", "enabled", "label"];

/// Collect the repeated `sub` nodes into a map. kdl-serde hands over a sequence
/// when a name repeats but a single node when it appears once, so both shapes are
/// accepted; `deserialize_struct` is what makes the positional `#0` visible.
fn deserialize_subs<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<SubKey, SubOverride>, D::Error> {
    struct SubVisitor;

    impl<'de> Visitor<'de> for SubVisitor {
        type Value = Vec<SubEntry>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("one or more `sub \"<key>\" { … }` nodes")
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            SubEntry::deserialize(MapAccessDeserializer::new(map)).map(|entry| vec![entry])
        }

        fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
            Vec::<SubEntry>::deserialize(SeqAccessDeserializer::new(seq))
        }
    }

    let entries = d.deserialize_struct("sub", SUB_FIELDS, SubVisitor)?;
    Ok(entries
        .into_iter()
        .map(|e| {
            let default = SubOverride::default();
            (
                e.key,
                SubOverride {
                    enabled: e.enabled.unwrap_or(default.enabled),
                    label: e.label,
                },
            )
        })
        .collect())
}

/// One `pool "<name>" { … }` node.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct PoolEntry {
    #[serde(rename = "#0")]
    name: String,
    provider: Option<Provider>,
    #[serde(default, deserialize_with = "deserialize_members")]
    subs: Vec<String>,
    /// The spelling that can name *which* subscription of an account it means.
    #[serde(rename = "sub", default, deserialize_with = "deserialize_pool_subs")]
    sub: Vec<PoolSubEntry>,
    max_sub_session_utilization: Option<f32>,
    max_sub_weekly_utilization: Option<f32>,
}

/// One `sub [<provider>] "<id>"` line inside a `pool`: with two arguments the
/// first is the provider. Read as a struct (`#0`/`#1`) because kdl-serde would
/// otherwise flatten it into the same shape as two one-argument nodes.
#[derive(Debug, Deserialize)]
struct PoolSubEntry {
    #[serde(rename = "#0")]
    first: String,
    #[serde(rename = "#1")]
    second: Option<String>,
}

const POOL_SUB_FIELDS: &[&str] = &["#0", "#1"];

impl PoolSubEntry {
    fn into_member(self) -> Result<PoolMember> {
        let Some(id) = self.second else {
            return Ok(PoolMember::any(self.first.trim()));
        };
        let name = self.first.trim();
        let provider = Provider::ALL
            .into_iter()
            .find(|p| p.id().eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                Error::config(format!(
                    "a pool's `sub {name} {id:?}` names no provider: write \
                     `sub claude {id:?}`, `sub codex {id:?}`, or drop the \
                     first word for either"
                ))
            })?;
        Ok(PoolMember {
            provider: Some(provider),
            id: id.trim().to_owned(),
        })
    }
}

/// One-or-many, plus the bare-node `false` kdl-serde produces for an absent child.
fn deserialize_pool_subs<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<PoolSubEntry>, D::Error> {
    struct PoolSubVisitor;

    impl<'de> Visitor<'de> for PoolSubVisitor {
        type Value = Vec<PoolSubEntry>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("one or more `sub [<provider>] \"<id>\"` nodes")
        }

        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            PoolSubEntry::deserialize(MapAccessDeserializer::new(map)).map(|entry| vec![entry])
        }

        fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
            Vec::<PoolSubEntry>::deserialize(SeqAccessDeserializer::new(seq))
        }
    }

    d.deserialize_struct("sub", POOL_SUB_FIELDS, PoolSubVisitor)
}

/// The `subs` list in every shape kdl-serde produces: a sequence, a single
/// scalar, or `false` for an absent node — a `Vec` has no `None` to land in, so
/// that last one is read as "no members listed", which means *every* sub.
fn deserialize_members<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    struct MemberVisitor;

    impl<'de> Visitor<'de> for MemberVisitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("`subs \"a@x.com\" \"b@x.com\"`")
        }

        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
            Ok(vec![v.to_owned()])
        }

        fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
            Vec::<String>::deserialize(SeqAccessDeserializer::new(seq))
        }
    }

    d.deserialize_any(MemberVisitor)
}

const POOL_FIELDS: &[&str] = &[
    "#0",
    "provider",
    "subs",
    "sub",
    "max-sub-session-utilization",
    "max-sub-weekly-utilization",
];

/// Collect the repeated `pool` nodes, preserving file order — it is tab order.
fn deserialize_pools<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<PoolConfig>, D::Error> {
    struct PoolVisitor;

    impl<'de> Visitor<'de> for PoolVisitor {
        type Value = Vec<PoolEntry>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("one or more `pool \"<name>\" { … }` nodes")
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            PoolEntry::deserialize(MapAccessDeserializer::new(map)).map(|entry| vec![entry])
        }

        fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
            Vec::<PoolEntry>::deserialize(SeqAccessDeserializer::new(seq))
        }
    }

    let entries = d.deserialize_struct("pool", POOL_FIELDS, PoolVisitor)?;
    entries
        .into_iter()
        .map(|e| {
            let default = PoolConfig::default();
            // one list written two ways, so they concatenate
            let mut subs: Vec<PoolMember> = e
                .subs
                .into_iter()
                .map(|id| PoolMember::any(id.trim()))
                .collect();
            for entry in e.sub {
                subs.push(entry.into_member().map_err(serde::de::Error::custom)?);
            }
            Ok(PoolConfig {
                name: e.name.trim().to_owned(),
                provider: e.provider,
                subs,
                max_sub_session_utilization: e
                    .max_sub_session_utilization
                    .unwrap_or(default.max_sub_session_utilization),
                max_sub_weekly_utilization: e
                    .max_sub_weekly_utilization
                    .unwrap_or(default.max_sub_weekly_utilization),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests_support::temp_dir;

    /// The README's example, plus a `redact-emails` line an older subbier wrote.
    const README_EXAMPLE: &str = r##"proxy {
    enabled #true
    bind "127.0.0.1:8787"
    // key "some-secret"        // required when bind is not loopback
    strategy "round-robin"      // lowest-usage | highest-usage | round-robin | least-connections
    auto-switch #true
    // sticky #true             // unset = the strategy's default
    codex #true
    claude #true
}

poll {
    interval "180s"             // usage refresh cadence
    usage-timeout "5s"          // round-level deadline for a selection usage round
}

ui {
    redact-emails #false        // deleted; read by nothing, an error to nobody
    warn-pct 75
    critical-pct 90
    notifications #false
    menu-bar "icon-percent"     // icon-percent | icon | percent
    launch-at-login #false
}

history {
    retain-days 7
}

// optional per-sub overrides, keyed by the stable sub key (provider:account-id)
sub "codex:4575f150-…" {
    enabled #true
    label "work"
}
"##;

    #[test]
    fn a_missing_or_empty_file_is_a_fully_working_install() {
        let dir = temp_dir("config-missing");
        let config = Config::load_from(&dir.join("config.kdl")).unwrap();
        assert_eq!(config, Config::default());
        config.validate().unwrap();

        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert_eq!(
            Config::parse("// only a comment\n").unwrap(),
            Config::default()
        );
    }

    /// The copied-from-the-docs example must land on exactly the defaults.
    #[test]
    fn the_readme_example_parses_to_the_defaults() {
        let config = Config::parse(README_EXAMPLE).unwrap();
        let key = SubKey("codex:4575f150-…".into());

        assert_eq!(
            config,
            Config {
                subs: BTreeMap::from([(
                    key.clone(),
                    SubOverride {
                        enabled: true,
                        label: Some("work".into()),
                    }
                )]),
                ..Config::default()
            }
        );
        // commented-out is unset, which is not the same as `#false`
        assert!(config.proxy.key.is_none());
        assert!(config.proxy.sticky.is_none());
        assert_eq!(config.sub_label(&key), Some("work"));
    }

    /// A block that mentions one key must not read as "off" for every key it omits.
    #[test]
    fn an_unmentioned_key_keeps_its_default_rather_than_going_false() {
        let config = Config::parse("ui {\n    warn-pct 60\n}\n").expect("parse");
        assert_eq!(config.ui.warn_pct, 60.0);
        assert_eq!(config.ui.menu_bar, MenuBarStyle::IconPercent);
        assert!(config.proxy.enabled && config.proxy.codex && config.proxy.claude);

        // a bare node is still the flag KDL says it is
        assert!(
            Config::parse("ui {\n    notifications\n}\n")
                .expect("parse")
                .ui
                .notifications
        );
        assert!(Config::parse("ui {\n    menu-bar \"inbox\"\n}\n").is_err());
    }

    #[test]
    fn an_unknown_key_is_never_a_parse_error() {
        for text in [
            "ui {\n    show-emails #false\n    warn-pct 60\n}\n",
            "ui {\n    redact-emails #true\n    warn-pct 60\n}\n",
        ] {
            let config = Config::parse(text).expect("an unknown key is never a parse error");
            assert_eq!(
                config.ui.warn_pct, 60.0,
                "the rest of the block still reads"
            );
            let doc = text.parse::<KdlDocument>().expect("kdl");
            assert!(retired_keys(&doc).is_empty(), "{:?}", retired_keys(&doc));
        }
    }

    #[test]
    fn several_sub_nodes_all_land_in_the_map() {
        let config = Config::parse(
            r#"
            sub "codex:one" { enabled #false }
            sub "claude:two" { label "personal" }
            sub "claude:three"
            "#,
        )
        .unwrap();

        assert_eq!(config.subs.len(), 3);
        assert!(!config.sub_enabled(&SubKey("codex:one".into())));
        assert_eq!(
            config.sub_label(&SubKey("claude:two".into())),
            Some("personal")
        );
        // mentioned with no body: enabled, unlabelled
        assert!(config.sub_enabled(&SubKey("claude:three".into())));
        assert_eq!(config.sub_label(&SubKey("claude:three".into())), None);
        assert!(config.sub_enabled(&SubKey("codex:unknown".into())));
    }

    #[test]
    fn durations_are_written_the_way_a_human_writes_them() {
        let config =
            Config::parse(r#"poll { interval "1h 30m"; usage-timeout "1500ms" }"#).unwrap();
        assert_eq!(config.poll.interval, SignedDuration::from_secs(5400));
        assert_eq!(config.poll.usage_timeout, SignedDuration::from_millis(1500));
        assert!(Config::parse(r#"poll { interval "soon" }"#).is_err());
    }

    /// Binding off loopback without a key would expose every logged-in subscription to the LAN.
    #[test]
    fn a_non_loopback_bind_without_a_key_is_rejected_by_name() {
        let message = Config::parse(r#"proxy { bind "0.0.0.0:8787" }"#)
            .unwrap_err()
            .to_string();
        assert!(message.contains("proxy.bind"), "{message}");
        assert!(message.contains("proxy.key"), "{message}");

        Config::parse(r#"proxy { bind "0.0.0.0:8787"; key "hunter2" }"#)
            .expect("a key makes it legal");

        for bind in [
            "127.0.0.1:8787",
            "127.0.0.5:1",
            "[::1]:8787",
            "localhost:8787",
        ] {
            Config::parse(&format!(r#"proxy {{ bind "{bind}" }}"#))
                .unwrap_or_else(|e| panic!("{bind} should be loopback: {e}"));
        }
        // unprovable and malformed binds are errors, not silent fallbacks
        for bind in [
            "192.168.1.10:8787",
            "example.com:8787",
            "[::]:8787",
            "8787",
            "127.0.0.1:http",
        ] {
            assert!(
                Config::parse(&format!(r#"proxy {{ bind "{bind}" }}"#)).is_err(),
                "{bind}"
            );
        }
    }

    #[test]
    fn sticky_defaults_per_strategy_but_an_explicit_value_wins() {
        let sticky = |text: &str| Config::parse(text).unwrap().proxy.effective_sticky();
        assert!(sticky(r#"proxy { strategy "lowest-usage" }"#));
        assert!(!sticky(r#"proxy { strategy "round-robin" }"#));
        assert!(!sticky(
            r#"proxy { strategy "lowest-usage"; sticky #false }"#
        ));
        assert!(sticky(r#"proxy { strategy "round-robin"; sticky #true }"#));
        assert_eq!(
            Config::parse(r#"proxy { strategy "round-robin" }"#)
                .unwrap()
                .proxy
                .sticky,
            None,
            "unset stays unset, so the menu can express all three states"
        );
    }

    #[test]
    fn a_file_that_exists_but_does_not_parse_is_an_error() {
        let dir = temp_dir("config-broken");
        let path = dir.join("config.kdl");
        for text in [
            "proxy { strategy \"no-such-strategy\" }\n",
            "proxy { enabled #true\n",
        ] {
            std::fs::write(&path, text).unwrap();
            assert!(Config::load_from(&path).is_err(), "{text}");
        }
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    const POOLS: &str = r#"
proxy { strategy "round-robin" }

pool "moonshot" {
    provider "codex"
    subs "a@x.com" "b@x.com"
    max-sub-weekly-utilization 0.5
    max-sub-session-utilization 0.5
}

pool "critical" {
    subs "c@x.com"
}

plan-weights {
    "codex:pro" 6
}
"#;

    fn key(s: &str) -> SubKey {
        SubKey(s.to_string())
    }

    fn pool_of(subs: Vec<PoolMember>) -> PoolConfig {
        PoolConfig {
            subs,
            ..PoolConfig::default()
        }
    }

    #[test]
    fn a_pool_reads_every_field_and_keeps_file_order() {
        let config = Config::parse(POOLS).expect("the document parses");
        let names: Vec<&str> = config.pools.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["moonshot", "critical"], "tab order is file order");

        let pool = config.pool("moonshot").expect("found by name");
        assert_eq!(pool.provider, Some(Provider::Codex));
        assert_eq!(
            pool.subs,
            [PoolMember::any("a@x.com"), PoolMember::any("b@x.com")]
        );
        // The ceilings are handed on in the crate's usual 0..=100 unit.
        assert_eq!(pool.max_session_pct(), 50.0);
        assert_eq!(pool.max_weekly_pct(), 50.0);

        let bare = config.pool("critical").unwrap();
        assert_eq!(bare.max_session_pct(), 100.0, "unset is no ceiling");
        assert_eq!(bare.provider, None, "unset serves both providers");

        assert_eq!(config.plan_weights.get("codex:pro"), Some(&6.0));

        // found case-insensitively, as the URL it is served on is matched
        assert!(config.pool("MoonShot").is_some());
        assert!(config.pool(" moonshot ").is_some());
        assert!(config.pool("nope").is_none());
    }

    /// kdl-serde hands over a bare node for one pool and a sequence for several; both must work.
    #[test]
    fn one_pool_parses_as_well_as_several_and_none_at_all() {
        let config = Config::parse("pool \"only\" { subs \"a@x\" }").unwrap();
        assert_eq!(config.pools.len(), 1);
        assert_eq!(config.pools[0].subs, [PoolMember::any("a@x")]);
        assert!(
            Config::parse("proxy { enabled #true }")
                .unwrap()
                .pools
                .is_empty()
        );
    }

    #[test]
    fn a_member_matches_by_email_key_label_or_bare_account_id() {
        let by_email = pool_of(vec![PoolMember::any("a@x.com")]);
        assert!(by_email.matches(&key("codex:acct-1"), "work", Some("a@x.com")));
        assert!(
            by_email.matches(&key("codex:acct-1"), "A@X.COM", None),
            "label, folded"
        );
        assert!(!by_email.matches(&key("codex:acct-1"), "work", Some("b@x.com")));

        assert!(pool_of(vec![PoolMember::any("codex:acct-1")]).matches(
            &key("codex:acct-1"),
            "work",
            None
        ));
        assert!(pool_of(vec![PoolMember::any("acct-1")]).matches(
            &key("codex:acct-1"),
            "work",
            None
        ));
    }

    /// One node with two arguments and two nodes with one each are one shape to kdl-serde.
    #[test]
    fn a_sub_line_may_name_the_provider_of_the_account_it_means() {
        let config = Config::parse(
            r#"pool "mixed" {
                   sub claude "a@x.com"
                   sub codex "b@x.com"
                   sub "c@x.com"
               }"#,
        )
        .expect("the document parses");
        let pool = config.pool("mixed").unwrap();
        assert_eq!(
            pool.subs,
            [
                PoolMember {
                    provider: Some(Provider::Claude),
                    id: "a@x.com".to_owned()
                },
                PoolMember {
                    provider: Some(Provider::Codex),
                    id: "b@x.com".to_owned()
                },
                PoolMember::any("c@x.com"),
            ]
        );

        // and it narrows that member alone, which a pool-wide `provider` cannot
        assert!(pool.matches(&key("claude:acct-1"), "work", Some("a@x.com")));
        assert!(!pool.matches(&key("codex:acct-1"), "work", Some("a@x.com")));
        assert!(pool.matches(&key("codex:acct-2"), "work", Some("b@x.com")));
        assert!(!pool.matches(&key("claude:acct-2"), "work", Some("b@x.com")));
    }

    /// Both spellings of the member list add up rather than one silently winning.
    #[test]
    fn subs_and_sub_lines_are_one_list() {
        let config = Config::parse(
            r#"pool "p" {
                   subs "a@x.com" "b@x.com"
                   sub codex "c@x.com"
               }"#,
        )
        .unwrap();
        let pool = config.pool("p").unwrap();
        assert_eq!(pool.subs.len(), 3);
        assert!(pool.matches(&key("claude:acct-1"), "work", Some("a@x.com")));
        assert!(pool.matches(&key("codex:acct-3"), "work", Some("c@x.com")));
        assert!(!pool.matches(&key("claude:acct-3"), "work", Some("c@x.com")));
    }

    #[test]
    fn an_empty_member_list_admits_everything_the_provider_filter_allows() {
        let pool = PoolConfig {
            provider: Some(Provider::Codex),
            ..PoolConfig::default()
        };
        assert!(pool.matches(&key("codex:acct-1"), "work", None));
        assert!(!pool.matches(&key("claude:acct-2"), "work", None));

        // a contradiction narrows rather than silently widening the pool
        let contradictory = PoolConfig {
            provider: Some(Provider::Codex),
            subs: vec![PoolMember::any("a@x.com")],
            ..PoolConfig::default()
        };
        assert!(!contradictory.matches(&key("claude:acct-2"), "work", Some("a@x.com")));
    }

    /// A duplicate name is one route with two configs, and a first word that is not a provider is a typo.
    #[test]
    fn a_pool_that_cannot_mean_what_it_says_is_rejected_with_advice() {
        for bad in ["a/b", "a b", "a#b", "a%b", ""] {
            let text = format!("pool \"{bad}\" {{ subs \"a@x\" }}");
            assert!(Config::parse(&text).is_err(), "pool name {bad:?}");
        }
        assert!(Config::parse("pool \"p\" { subs \"a\" }\npool \"P\" { subs \"b\" }").is_err());

        for (text, advice) in [
            (
                "pool \"p\" { sub anthropic \"a@x.com\" }",
                "names no provider",
            ),
            (
                "pool \"p\" { max-sub-weekly-utilization 50 }",
                "0.5 for half, not 50",
            ),
        ] {
            let message = Config::parse(text).unwrap_err().to_string();
            assert!(message.contains(advice), "{message}");
        }
    }
}
