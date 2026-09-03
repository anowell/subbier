//! Provider-free domain types. Every provider unit difference dies inside
//! `provider/{codex,claude}.rs`; here every timestamp is a [`jiff::Timestamp`]
//! and every duration a [`jiff::SignedDuration`].

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::error::Error;

/// In-process handle for a subscription: stable for the lifetime of one engine
/// process and never persisted. Config and sqlite reference [`SubKey`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubId(pub u32);

impl fmt::Display for SubId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The persisted identity of a subscription: `"{provider}:{account_id}"`,
/// falling back to `"{provider}:{email}"` when the provider names no account id.
/// `subs.json`, `config.kdl`'s `sub "…"` nodes and both sqlite tables key on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubKey(pub String);

impl SubKey {
    pub fn new(provider: Provider, account: impl fmt::Display) -> Self {
        Self(format!("{}:{account}", provider.id()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The provider half of the key, if it is one we know.
    #[must_use]
    pub fn provider(&self) -> Option<Provider> {
        self.0.split_once(':').and_then(|(p, _)| p.parse().ok())
    }

    /// The account half of the key (everything after the first `:`).
    #[must_use]
    pub fn account(&self) -> &str {
        self.0.split_once(':').map_or("", |(_, a)| a)
    }
}

impl fmt::Display for SubKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SubKey {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// The two providers subbier fronts. There will only ever be two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Codex,
    Claude,
}

impl Provider {
    /// Every provider, in the order [`Provider::index`] assigns.
    pub const ALL: [Provider; 2] = [Provider::Codex, Provider::Claude];

    /// The canonical lowercase id: the spelling used in [`SubKey`],
    /// `config.kdl`, the sqlite `provider` column and the JSON serialisation.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Provider::Codex => "codex",
            Provider::Claude => "claude",
        }
    }

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Provider::Codex => "Codex",
            Provider::Claude => "Claude",
        }
    }

    /// Index into fixed-size per-provider arrays such as
    /// [`crate::snapshot::SettingsView::providers_proxied`].
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Provider::Codex => 0,
            Provider::Claude => 1,
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

impl FromStr for Provider {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "codex" => Ok(Provider::Codex),
            "claude" => Ok(Provider::Claude),
            other => Err(Error::config(format!("unknown provider {other:?}"))),
        }
    }
}

/// How alarming a usage percentage is; [`crate::severity`] classifies.
///
/// `Ord` is load-bearing: `worst` is a `max()` over enabled subs, and
/// `notification_transition` only notifies on a strictly upward band change.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Ok,
    Warn,
    Critical,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Severity::Ok => "ok",
            Severity::Warn => "warn",
            Severity::Critical => "critical",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which allowance window a number belongs to. The string form is both the
/// serde representation and the sqlite `allowance_sample.window` column, so a
/// scoped window named `"session"` would round-trip as the built-in variant.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
pub enum WindowKind {
    /// The short rolling window. 5h on both providers.
    Session,
    Weekly,
    /// A per-model or otherwise narrowed limit, e.g. weekly Fable-only.
    Scoped(String),
}

impl WindowKind {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            WindowKind::Session => "session",
            WindowKind::Weekly => "weekly",
            WindowKind::Scoped(name) => name,
        }
    }
}

impl fmt::Display for WindowKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<String> for WindowKind {
    fn from(s: String) -> Self {
        match s.as_str() {
            "session" => WindowKind::Session,
            "weekly" => WindowKind::Weekly,
            _ => WindowKind::Scoped(s),
        }
    }
}

impl From<&str> for WindowKind {
    fn from(s: &str) -> Self {
        Self::from(s.to_owned())
    }
}

impl From<WindowKind> for String {
    fn from(k: WindowKind) -> Self {
        match k {
            WindowKind::Scoped(name) => name,
            other => other.as_str().to_owned(),
        }
    }
}

/// One allowance window as reported by a provider usage API.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// Percentage of the allowance consumed, `0..=100`. Missing upstream => `0`.
    pub pct: f32,
    pub resets_at: Option<Timestamp>,
    /// Derived, and not always known — Codex gives `reset_at -
    /// limit_window_seconds` exactly, Claude only permits `resets_at -
    /// nominal_width`. `None` means no projection; never guess.
    pub started_at: Option<Timestamp>,
}

impl UsageWindow {
    #[must_use]
    pub const fn from_pct(pct: f32) -> Self {
        Self {
            pct,
            resets_at: None,
            started_at: None,
        }
    }
}

/// A provider's complete usage report for one subscription, normalised.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// `"plus"`, `"max20"`, `"team"`, …
    pub plan: Option<String>,
    /// The short (5h) window.
    pub session: Option<UsageWindow>,
    pub weekly: Option<UsageWindow>,
    /// Narrower limits, keyed by their upstream name.
    pub scoped: Vec<(String, UsageWindow)>,
    /// The provider's own verdict on whether this account is cut off, tri-state
    /// on purpose: `None` — the provider did not say — is not `Some(false)`.
    /// A percentage can lag enforcement, so `Some(true)` beats `pct` (see
    /// [`crate::balance::is_exhausted`]). Never a fetch failure — that is `Err`.
    pub limit_reached: Option<bool>,
}

/// A linear burn-rate projection to 100% of a window, from
/// [`crate::pace::project`]. Only exists when it lands strictly before the
/// window's reset; a projection past the reset is not actionable and is withheld.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Projection {
    pub exhausts_at: Timestamp,
    /// `exhausts_at - now`.
    pub until_exhaustion: SignedDuration,
}

/// OAuth material for one subscription. [`fmt::Debug`] is hand-written and
/// redacts both tokens; anything embedding this must go through that impl.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access: String,
    pub refresh: Option<String>,
    /// Codex derives it from the access token's JWT `exp` claim; Claude from
    /// `now + expires_in`.
    pub expires_at: Option<Timestamp>,
}

impl Tokens {
    /// An unknown `expires_at` counts as expired, so a refresh is attempted
    /// rather than a doomed request sent.
    #[must_use]
    pub fn is_expired(&self, now: Timestamp, skew: SignedDuration) -> bool {
        match self.expires_at {
            Some(exp) => exp - skew <= now,
            None => true,
        }
    }
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("access", &"<redacted>")
            .field("refresh", &self.refresh.as_ref().map(|_| "<redacted>"))
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Where a set of credentials came from. Adopted credentials are read-mostly:
/// subbier reads `~/.codex/auth.json` and the macOS Keychain but never writes
/// refreshed tokens back to them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CredentialSource {
    /// Read out of another tool's on-disk credential file.
    Adopted {
        from: PathBuf,
    },
    Keychain,
    /// Obtained by subbier's own OAuth flow.
    Subbier,
}

/// Everything needed to talk to a provider as one account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Credentials {
    /// Codex: `chatgpt_account_id` from the id token. Claude: organization id.
    pub account_id: Option<String>,
    pub email: Option<String>,
    /// The tier as the *credential source* spelled it. Claude keeps
    /// `subscriptionType` in its keychain blob, the only place a Claude plan is
    /// stated at all; Codex leaves this `None` and states its plan on the usage
    /// endpoint instead ([`Usage::plan`]).
    #[serde(default)]
    pub plan: Option<String>,
    /// Flattened, so `subs.json` is one flat record rather than a nested one.
    #[serde(flatten)]
    pub tokens: Tokens,
    pub source: CredentialSource,
}

/// A subscription as persisted in `subs.json` (mode 0600); `store/creds.rs`
/// owns the file, its atomic write and its permissions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sub {
    pub key: SubKey,
    pub provider: Provider,
    /// Display label — an email or org name.
    pub label: String,
    #[serde(flatten)]
    pub credentials: Credentials,
}

/// What the macOS menu bar item shows (`ui.menu-bar`). The icon is a template
/// image, so macOS discards its colour and severity has to ride on the
/// percentage text — which is why the default shows both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MenuBarStyle {
    #[default]
    IconPercent,
    Icon,
    Percent,
}

impl MenuBarStyle {
    /// Every style, in menu order.
    pub const ALL: [MenuBarStyle; 3] = [
        MenuBarStyle::IconPercent,
        MenuBarStyle::Icon,
        MenuBarStyle::Percent,
    ];

    /// The kebab-case config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            MenuBarStyle::IconPercent => "icon-percent",
            MenuBarStyle::Icon => "icon",
            MenuBarStyle::Percent => "percent",
        }
    }

    #[must_use]
    pub const fn shows_icon(self) -> bool {
        matches!(self, MenuBarStyle::IconPercent | MenuBarStyle::Icon)
    }

    #[must_use]
    pub const fn shows_percent(self) -> bool {
        matches!(self, MenuBarStyle::IconPercent | MenuBarStyle::Percent)
    }
}

impl fmt::Display for MenuBarStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the router *ranks* candidate subs. Stickiness is a separate axis
/// ([`StrategyKind::default_sticky`] and the `proxy.sticky` config key).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StrategyKind {
    /// Fewest percent of allowance used.
    LowestUsage,
    /// Most percent used, among candidates below 100 — drain one account fully
    /// before touching the next.
    HighestUsage,
    /// Rotate through candidates ordered by [`SubId`].
    #[default]
    RoundRobin,
    /// Fewest **proxy-observed** in-flight requests. It cannot see traffic that
    /// bypassed the proxy, and that is correct, not a bug.
    LeastConnections,
}

impl StrategyKind {
    /// Every strategy, in menu order.
    pub const ALL: [StrategyKind; 4] = [
        StrategyKind::LowestUsage,
        StrategyKind::HighestUsage,
        StrategyKind::RoundRobin,
        StrategyKind::LeastConnections,
    ];

    /// The kebab-case config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            StrategyKind::LowestUsage => "lowest-usage",
            StrategyKind::HighestUsage => "highest-usage",
            StrategyKind::RoundRobin => "round-robin",
            StrategyKind::LeastConnections => "least-connections",
        }
    }

    /// Whether this strategy is sticky when `proxy.sticky` is unset.
    ///
    /// True for the two ranking strategies: a ranking says nothing about whether
    /// a mid-conversation request may hop accounts, and Codex reasoning items
    /// carry account-scoped `encrypted_content`.
    #[must_use]
    pub const fn default_sticky(self) -> bool {
        matches!(self, StrategyKind::LowestUsage | StrategyKind::HighestUsage)
    }
}

impl fmt::Display for StrategyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renaming any of these spellings is a breaking change for a user's config.
    #[test]
    fn config_spellings_agree_with_serde() {
        fn quoted(s: impl fmt::Display) -> String {
            format!("\"{s}\"")
        }
        for kind in StrategyKind::ALL {
            let json = quoted(kind);
            assert_eq!(serde_json::to_string(&kind).unwrap(), json);
            assert_eq!(serde_json::from_str::<StrategyKind>(&json).unwrap(), kind);
        }
        for style in MenuBarStyle::ALL {
            let json = quoted(style);
            assert_eq!(serde_json::to_string(&style).unwrap(), json);
            assert_eq!(serde_json::from_str::<MenuBarStyle>(&json).unwrap(), style);
        }
        for (i, provider) in Provider::ALL.into_iter().enumerate() {
            assert_eq!(serde_json::to_string(&provider).unwrap(), quoted(provider));
            assert_eq!(provider.id().parse::<Provider>().unwrap(), provider);
            // `SettingsView::providers_proxied` indexes by this.
            assert_eq!(provider.index(), i);
        }
        assert_eq!(serde_json::to_string(&Severity::Warn).unwrap(), "\"warn\"");
        assert!("openai".parse::<Provider>().is_err());
    }

    #[test]
    fn default_sticky_is_true_only_for_the_ranking_strategies() {
        assert!(StrategyKind::LowestUsage.default_sticky());
        assert!(StrategyKind::HighestUsage.default_sticky());
        assert!(!StrategyKind::RoundRobin.default_sticky());
        assert!(!StrategyKind::LeastConnections.default_sticky());
    }

    #[test]
    fn a_sub_key_composes_and_decomposes() {
        let key = SubKey::new(Provider::Codex, "4575f150-abc");
        assert_eq!(key.as_str(), "codex:4575f150-abc");
        assert_eq!(key.provider(), Some(Provider::Codex));
        assert_eq!(key.account(), "4575f150-abc");

        // Only the first colon splits, so an account id may contain one.
        let key = SubKey::new(Provider::Claude, "a:b");
        assert_eq!(key.account(), "a:b");
        assert_eq!(SubKey("garbage".into()).provider(), None);
    }

    #[test]
    fn severity_orders_from_ok_to_critical() {
        assert_eq!(
            [Severity::Ok, Severity::Critical, Severity::Warn]
                .into_iter()
                .max()
                .unwrap(),
            Severity::Critical
        );
    }

    #[test]
    fn window_kind_round_trips_through_its_string_form() {
        for (kind, text) in [
            (WindowKind::Session, "session"),
            (WindowKind::Weekly, "weekly"),
            (WindowKind::Scoped("fable".into()), "fable"),
        ] {
            assert_eq!(kind.as_str(), text);
            assert_eq!(WindowKind::from(text), kind);
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{text}\""));
            assert_eq!(serde_json::from_str::<WindowKind>(&json).unwrap(), kind);
        }
    }

    #[test]
    fn tokens_debug_redacts_both_secrets_even_through_a_wrapper() {
        let tokens = Tokens {
            access: "sk-access-supersecret".into(),
            refresh: Some("sk-refresh-supersecret".into()),
            expires_at: Some(Timestamp::UNIX_EPOCH),
        };
        let rendered = format!("{tokens:?}");
        assert!(!rendered.contains("supersecret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");

        let creds = Credentials {
            plan: None,
            account_id: Some("acct".into()),
            email: None,
            tokens,
            source: CredentialSource::Subbier,
        };
        assert!(!format!("{creds:?}").contains("supersecret"));
    }

    #[test]
    fn an_unknown_token_expiry_counts_as_expired() {
        let now = Timestamp::UNIX_EPOCH;
        let skew = SignedDuration::from_secs(60);
        let at = |expires_at| Tokens {
            access: "a".into(),
            refresh: None,
            expires_at,
        };
        assert!(!at(Some(now + SignedDuration::from_secs(3600))).is_expired(now, skew));
        assert!(at(Some(now + SignedDuration::from_secs(30))).is_expired(now, skew));
        assert!(at(None).is_expired(now, skew));
    }

    #[test]
    fn sub_serialises_as_a_flat_record() {
        let sub = Sub {
            key: SubKey::new(Provider::Codex, "acct-1"),
            provider: Provider::Codex,
            label: "work".into(),
            credentials: Credentials {
                plan: None,
                account_id: Some("acct-1".into()),
                email: Some("me@example.com".into()),
                tokens: Tokens {
                    access: "at".into(),
                    refresh: Some("rt".into()),
                    expires_at: Some(Timestamp::UNIX_EPOCH),
                },
                source: CredentialSource::Adopted {
                    from: PathBuf::from("/home/me/.codex/auth.json"),
                },
            },
        };

        let value: serde_json::Value = serde_json::to_value(&sub).unwrap();
        for field in [
            "key",
            "provider",
            "label",
            "account_id",
            "email",
            "access",
            "refresh",
            "expires_at",
            "source",
        ] {
            assert!(value.get(field).is_some(), "missing {field} in {value}");
        }
        assert_eq!(value["source"]["kind"], "adopted");
        assert_eq!(serde_json::from_value::<Sub>(value).unwrap(), sub);
    }
}
