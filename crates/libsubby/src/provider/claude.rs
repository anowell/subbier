//! Claude usage parsing. Every Anthropic unit quirk dies here; downstream sees
//! only [`crate::model::Usage`]. Two sources of the same numbers: the
//! `/api/oauth/usage` body ([`parse_usage`]) and the unified rate-limit headers
//! on every Messages response ([`parse_unified_headers`]).

use jiff::{SignedDuration, Timestamp};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;
use tokio::time::Instant;

use super::{get_text, normalise_pct};
use crate::error::{Error, Result};
use crate::model::{Credentials, Provider, Usage, UsageWindow};

/// The OAuth beta the usage endpoint requires.
const OAUTH_BETA: (&str, &str) = ("anthropic-beta", "oauth-2025-04-20");

/// Nominal width of the session window. Anthropic reports only `resets_at`, so
/// `started_at` is `resets_at - SESSION_WINDOW` — an assumption about the plan's
/// shape, not a number the API gave us. Contrast Codex, where it is exact.
pub const SESSION_WINDOW: SignedDuration = SignedDuration::from_hours(5);

/// Nominal width of the weekly window. Same caveat as [`SESSION_WINDOW`].
pub const WEEKLY_WINDOW: SignedDuration = SignedDuration::from_hours(24 * 7);

/// `GET {base}/api/oauth/usage`, normalised. Takes already-fresh credentials:
/// a 401 comes back for the caller to refresh and retry.
pub async fn fetch_usage(base: &str, c: &Credentials, deadline: Instant) -> Result<Usage> {
    let url = Provider::Claude.usage_url_from(base);
    let body = get_text(&url, headers(c)?, deadline).await?;
    parse_usage(&body)
}

fn headers(c: &Credentials) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    let mut bearer = HeaderValue::from_str(&format!("Bearer {}", c.tokens.access))
        .map_err(|_| Error::auth("claude access token is not a valid header value"))?;
    bearer.set_sensitive(true);
    headers.insert(AUTHORIZATION, bearer);
    headers.insert(OAUTH_BETA.0, HeaderValue::from_static(OAUTH_BETA.1));
    Ok(headers)
}

/// Parse an `/api/oauth/usage` body.
///
/// `limits[]` is preferred over the top-level `five_hour` / `seven_day` pair
/// because it is stable across plan shapes; the pair is only a fallback.
pub fn parse_usage(body: &str) -> Result<Usage> {
    let raw: RawUsage = serde_json::from_str(body)?;
    Ok(raw.normalise())
}

#[derive(Debug, Default, Deserialize)]
struct RawUsage {
    limits: Option<Vec<RawLimit>>,
    five_hour: Option<RawTopWindow>,
    seven_day: Option<RawTopWindow>,
}

impl RawUsage {
    fn normalise(self) -> Usage {
        let mut usage = Usage::default();
        let limits = self.limits.unwrap_or_default();
        for limit in &limits {
            let window = limit.normalise();
            match limit.kind.as_str() {
                "session" => usage.session = Some(window),
                "weekly_all" => usage.weekly = Some(window),
                _ => usage.scoped.push((limit.name(), window)),
            }
        }
        if limits.is_empty() {
            // The top-level pair spells the percentage `utilization`.
            usage.session = self.five_hour.map(|w| w.normalise(SESSION_WINDOW));
            usage.weekly = self.seven_day.map(|w| w.normalise(WEEKLY_WINDOW));
        }
        usage
    }
}

#[derive(Debug, Deserialize)]
struct RawLimit {
    kind: String,
    /// `"session"` | `"weekly"`. Says which nominal width applies.
    group: Option<String>,
    percent: Option<f32>,
    resets_at: Option<String>,
    scope: Option<RawScope>,
}

impl RawLimit {
    /// `scope.model.display_name` lowercased, falling back to `kind`.
    fn name(&self) -> String {
        self.scope
            .as_ref()
            .and_then(|s| s.model.as_ref())
            .and_then(|m| m.display_name.as_deref())
            .filter(|d| !d.is_empty())
            .unwrap_or(&self.kind)
            .to_lowercase()
    }

    fn width(&self) -> Option<SignedDuration> {
        let hint = self.group.as_deref().unwrap_or(&self.kind);
        if hint.starts_with("session") {
            Some(SESSION_WINDOW)
        } else if hint.starts_with("weekly") {
            Some(WEEKLY_WINDOW)
        } else {
            None
        }
    }

    fn normalise(&self) -> UsageWindow {
        let resets_at = parse_timestamp(self.resets_at.as_deref());
        UsageWindow {
            pct: normalise_pct(self.percent),
            resets_at,
            // Nominal, not exact. Not derivable => None; never guess.
            started_at: derive_started_at(resets_at, self.width()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawScope {
    model: Option<RawModel>,
}

#[derive(Debug, Deserialize)]
struct RawModel {
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTopWindow {
    utilization: Option<f32>,
    resets_at: Option<String>,
}

impl RawTopWindow {
    fn normalise(self, width: SignedDuration) -> UsageWindow {
        let resets_at = parse_timestamp(self.resets_at.as_deref());
        UsageWindow {
            pct: normalise_pct(self.utilization),
            resets_at,
            started_at: derive_started_at(resets_at, Some(width)),
        }
    }
}

/// An ISO-8601 instant. A malformed timestamp costs one field, not the poll.
fn parse_timestamp(raw: Option<&str>) -> Option<Timestamp> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.parse::<Timestamp>() {
        Ok(ts) => Some(ts),
        Err(e) => {
            tracing::debug!(error = %e, "unparseable resets_at from Claude");
            None
        }
    }
}

fn derive_started_at(
    resets_at: Option<Timestamp>,
    width: Option<SignedDuration>,
) -> Option<Timestamp> {
    resets_at?.checked_sub(width?).ok()
}

/// Which unified rate-limit window a header pair describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnifiedWindowId {
    /// No window suffix: the overall verdict for the request.
    Overall,
    FiveHour,
    SevenDay,
    /// A seven-day OAuth-inference window the usage endpoint never surfaces.
    SevenDayOauthInference,
}

impl UnifiedWindowId {
    pub const ALL: [UnifiedWindowId; 4] = [
        UnifiedWindowId::Overall,
        UnifiedWindowId::FiveHour,
        UnifiedWindowId::SevenDay,
        UnifiedWindowId::SevenDayOauthInference,
    ];

    /// The header infix.
    const fn infix(self) -> &'static str {
        match self {
            UnifiedWindowId::Overall => "",
            UnifiedWindowId::FiveHour => "5h-",
            UnifiedWindowId::SevenDay => "7d-",
            UnifiedWindowId::SevenDayOauthInference => "7d_oi-",
        }
    }

    /// The name a scoped window takes in [`Usage::scoped`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            UnifiedWindowId::Overall => "overall",
            UnifiedWindowId::FiveHour => "5h",
            UnifiedWindowId::SevenDay => "7d",
            UnifiedWindowId::SevenDayOauthInference => "7d_oi",
        }
    }
}

/// The verdict a unified rate-limit header carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitStatus {
    Allowed,
    /// Room left, but close enough that upstream is warning us.
    AllowedWarning,
    /// Refused: this window is spent.
    Rejected,
    /// Something we have not seen before, kept verbatim rather than guessed at.
    Other(String),
}

impl RateLimitStatus {
    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "allowed" => RateLimitStatus::Allowed,
            "allowed_warning" => RateLimitStatus::AllowedWarning,
            "rejected" => RateLimitStatus::Rejected,
            other => RateLimitStatus::Other(other.to_owned()),
        }
    }

    #[must_use]
    pub fn is_rejected(&self) -> bool {
        matches!(self, RateLimitStatus::Rejected)
    }

    /// [`RateLimitStatus::Other`] is a header we could not read, so it can only
    /// mean "the provider did not say" — never "not cut off".
    fn is_understood(&self) -> bool {
        !matches!(self, RateLimitStatus::Other(_))
    }
}

/// One window's worth of `Anthropic-Ratelimit-Unified-*` headers.
#[derive(Debug, Clone, PartialEq)]
pub struct UnifiedWindow {
    pub id: UnifiedWindowId,
    pub status: RateLimitStatus,
    pub resets_at: Option<Timestamp>,
}

impl UnifiedWindow {
    /// Only a rejection is a number: `allowed` and `allowed_warning` say
    /// nothing about how much is left, so they claim no percentage.
    #[must_use]
    pub fn pct(&self) -> Option<f32> {
        self.status.is_rejected().then_some(100.0)
    }
}

/// The `Anthropic-Ratelimit-Unified-*` headers off one Messages response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UnifiedRateLimits {
    pub windows: Vec<UnifiedWindow>,
}

impl UnifiedRateLimits {
    /// Whether any window came back `rejected`.
    #[must_use]
    pub fn is_rejected(&self) -> bool {
        self.windows.iter().any(|w| w.status.is_rejected())
    }

    /// Anthropic's verdict in [`Usage::limit_reached`]'s tri-state form. `None`
    /// when no window stated a verdict we understand, since an unreadable
    /// status is "the provider did not say", not "not cut off".
    #[must_use]
    pub fn limit_reached(&self) -> Option<bool> {
        if self.is_rejected() {
            return Some(true);
        }
        self.windows
            .iter()
            .any(|w| w.status.is_understood())
            .then_some(false)
    }

    /// The signal as a [`Usage`], or `None` when the headers say nothing a
    /// snapshot could carry. The result is a replacement snapshot, never a
    /// patch: see [`crate::usage::UsageCache::observe`].
    #[must_use]
    pub fn to_usage(&self) -> Option<Usage> {
        let mut usage = Usage::default();
        let mut carried = false;
        for window in &self.windows {
            // The overall verdict is not a window.
            if window.id == UnifiedWindowId::Overall {
                continue;
            }
            let Some(built) = build(window) else { continue };
            carried = true;
            match window.id {
                UnifiedWindowId::SevenDay => usage.weekly = Some(built),
                UnifiedWindowId::FiveHour => usage.session = Some(built),
                _ => usage.scoped.push((window.id.as_str().to_owned(), built)),
            }
        }
        // The verdict rides along rather than gating the snapshot: it is the
        // provider saying outright what the percentages may not show yet.
        usage.limit_reached = self.limit_reached();
        carried.then_some(usage)
    }
}

/// A window that carries either a percentage or a reset, else nothing.
fn build(window: &UnifiedWindow) -> Option<UsageWindow> {
    let pct = window.pct();
    if pct.is_none() && window.resets_at.is_none() {
        return None;
    }
    let width = match window.id {
        UnifiedWindowId::FiveHour => Some(SESSION_WINDOW),
        UnifiedWindowId::SevenDay | UnifiedWindowId::SevenDayOauthInference => Some(WEEKLY_WINDOW),
        UnifiedWindowId::Overall => None,
    };
    Some(UsageWindow {
        pct: pct.unwrap_or(0.0),
        resets_at: window.resets_at,
        started_at: derive_started_at(window.resets_at, width),
    })
}

const UNIFIED_PREFIX: &str = "anthropic-ratelimit-unified-";

/// Parse the `Anthropic-Ratelimit-Unified-*` headers off a Messages response.
/// `None` when the response carries none of them — no signal, leave the
/// snapshot alone.
#[must_use]
pub fn parse_unified_headers(headers: &HeaderMap) -> Option<UnifiedRateLimits> {
    let mut windows = Vec::new();
    for id in UnifiedWindowId::ALL {
        let status = header_str(headers, &format!("{UNIFIED_PREFIX}{}status", id.infix()));
        let reset = header_str(headers, &format!("{UNIFIED_PREFIX}{}reset", id.infix()));
        if status.is_none() && reset.is_none() {
            continue;
        }
        windows.push(UnifiedWindow {
            id,
            status: status
                .map(|s| RateLimitStatus::parse(&s))
                .unwrap_or_else(|| RateLimitStatus::Other(String::new())),
            resets_at: reset.as_deref().and_then(parse_reset),
        });
    }
    (!windows.is_empty()).then_some(UnifiedRateLimits { windows })
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Epoch seconds in the wild; an ISO-8601 instant is accepted too rather than
/// being silently dropped.
fn parse_reset(raw: &str) -> Option<Timestamp> {
    if let Ok(seconds) = raw.parse::<i64>() {
        return Timestamp::from_second(seconds).ok();
    }
    parse_timestamp(Some(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real, abridged `/api/oauth/usage` body.
    const LIVE_BODY: &str = include_str!("fixtures/claude_usage.json");

    fn ts(iso: &str) -> Timestamp {
        iso.parse().unwrap()
    }

    #[test]
    fn the_live_verified_body_parses_from_the_limits_array() {
        let usage = parse_usage(LIVE_BODY).unwrap();

        let session = usage.session.expect("session window");
        assert_eq!(session.pct, 2.0);
        assert_eq!(
            session.resets_at,
            Some(ts("2026-08-27T02:10:00.312648+00:00"))
        );
        assert_eq!(
            session.started_at,
            Some(ts("2026-08-27T02:10:00.312648+00:00") - SESSION_WINDOW),
            "nominal 5h before the reset"
        );

        let weekly = usage.weekly.expect("weekly window");
        assert_eq!(weekly.pct, 42.0);
        assert_eq!(
            weekly.started_at,
            Some(ts("2026-08-31T11:00:00.312668+00:00") - WEEKLY_WINDOW)
        );

        // The endpoint states no verdict, so the tri-state stays `None`.
        assert_eq!(usage.limit_reached, None);
    }

    #[test]
    fn a_scoped_limit_is_named_for_its_model_or_else_its_kind() {
        let usage = parse_usage(LIVE_BODY).unwrap();
        assert_eq!(usage.scoped.len(), 1);
        let (name, window) = &usage.scoped[0];
        assert_eq!(name, "fable", "scope.model.display_name, lowercased");
        assert_eq!(window.pct, 41.0);
        // It must NOT have overwritten the weekly_all window.
        assert_eq!(usage.weekly.unwrap().pct, 42.0);

        let no_model = parse_usage(
            r#"{"limits":[{"kind":"weekly_Opus","group":"weekly","percent":5,"scope":null}]}"#,
        )
        .unwrap();
        assert_eq!(no_model.scoped[0].0, "weekly_opus");
    }

    #[test]
    fn a_start_is_never_guessed_without_a_reset_and_a_known_width() {
        let usage = parse_usage(r#"{"limits":[{"kind":"session","group":"session"}]}"#).unwrap();
        let session = usage.session.unwrap();
        assert_eq!(session.pct, 0.0);
        assert_eq!(session.resets_at, None);
        assert_eq!(session.started_at, None);

        // A reset, but a width we cannot classify.
        let usage = parse_usage(
            r#"{"limits":[{"kind":"monthly_credits","percent":3,"resets_at":"2026-08-31T11:00:00Z"}]}"#,
        )
        .unwrap();
        let (name, window) = &usage.scoped[0];
        assert_eq!(name, "monthly_credits");
        assert!(window.resets_at.is_some());
        assert_eq!(window.started_at, None);
    }

    #[test]
    fn the_top_level_pair_is_only_a_fallback() {
        // limits[] present => the pair is ignored entirely.
        let usage = parse_usage(
            r#"{"five_hour":{"utilization":99.0},"limits":[{"kind":"session","group":"session","percent":2}]}"#,
        )
        .unwrap();
        assert_eq!(usage.session.unwrap().pct, 2.0);

        // limits[] absent => fall back, reading `utilization`, not `percent`.
        let usage = parse_usage(
            r#"{"five_hour":{"utilization":7.5,"resets_at":"2026-08-27T02:10:00Z"},"seven_day":null}"#,
        )
        .unwrap();
        let session = usage.session.unwrap();
        assert_eq!(session.pct, 7.5);
        assert_eq!(
            session.started_at,
            Some(ts("2026-08-27T02:10:00Z") - SESSION_WINDOW)
        );
        assert_eq!(usage.weekly, None);
    }

    #[test]
    fn a_malformed_reset_costs_one_field_not_the_poll() {
        let usage =
            parse_usage(r#"{"limits":[{"kind":"session","percent":4,"resets_at":"soon"}]}"#)
                .unwrap();
        let session = usage.session.unwrap();
        assert_eq!(session.pct, 4.0);
        assert_eq!(session.resets_at, None);
    }

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn unified_headers_parse_every_window_including_7d_oi() {
        let headers = header_map(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("Anthropic-Ratelimit-Unified-5h-Status", "allowed_warning"),
            ("Anthropic-Ratelimit-Unified-5h-Reset", "1787798509"),
            ("Anthropic-Ratelimit-Unified-7d-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-7d-Reset", "1788290706"),
            ("Anthropic-Ratelimit-Unified-7d_oi-Status", "rejected"),
            (
                "Anthropic-Ratelimit-Unified-7d_oi-Reset",
                "2026-08-31T11:00:00Z",
            ),
        ]);
        let limits = parse_unified_headers(&headers).expect("a signal");
        assert_eq!(limits.windows.len(), 4);
        let five_hour = |w: &&UnifiedWindow| w.id == UnifiedWindowId::FiveHour;
        let five_hour = limits.windows.iter().find(five_hour).unwrap();
        assert_eq!(five_hour.status, RateLimitStatus::AllowedWarning);
        assert_eq!(
            five_hour.resets_at,
            Timestamp::from_second(1_787_798_509).ok()
        );
        assert!(limits.is_rejected());

        let usage = limits.to_usage().expect("a usage snapshot");
        // Allowed windows carry a reset but claim no percentage.
        assert_eq!(usage.session.unwrap().pct, 0.0);
        assert_eq!(usage.weekly.unwrap().pct, 0.0);
        // 7d_oi has no named home, so it becomes a scoped window.
        assert_eq!(usage.scoped.len(), 1);
        assert_eq!(usage.scoped[0].0, "7d_oi");
        assert_eq!(usage.scoped[0].1.pct, 100.0, "rejected is a real 100%");
        assert_eq!(
            usage.scoped[0].1.resets_at,
            Some(ts("2026-08-31T11:00:00Z"))
        );
        assert_eq!(usage.limit_reached, Some(true));
    }

    #[test]
    fn allowed_unified_headers_are_an_explicit_not_cut_off_verdict() {
        let limits = parse_unified_headers(&header_map(&[
            ("Anthropic-Ratelimit-Unified-5h-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-5h-Reset", "1787798509"),
            ("Anthropic-Ratelimit-Unified-7d-Status", "allowed_warning"),
            ("Anthropic-Ratelimit-Unified-7d-Reset", "1788290706"),
        ]))
        .expect("a signal");
        assert!(!limits.is_rejected());
        assert_eq!(limits.limit_reached(), Some(false));
        assert_eq!(limits.to_usage().unwrap().limit_reached, Some(false));
    }

    #[test]
    fn a_status_we_cannot_read_is_no_verdict_rather_than_a_clean_bill() {
        // Reset but no status: `Other("")`, which must not become `Some(false)`.
        let limits = parse_unified_headers(&header_map(&[(
            "Anthropic-Ratelimit-Unified-5h-Reset",
            "1787798509",
        )]))
        .expect("a signal");
        assert_eq!(limits.limit_reached(), None);
        assert_eq!(limits.to_usage().unwrap().limit_reached, None);
    }

    #[test]
    fn a_response_without_the_headers_is_no_signal() {
        assert!(
            parse_unified_headers(&header_map(&[("content-type", "application/json")])).is_none()
        );
        // The overall verdict is parsed but is not a window, so there is
        // nothing to replace a snapshot with.
        let limits = parse_unified_headers(&header_map(&[(
            "anthropic-ratelimit-unified-status",
            "allowed",
        )]))
        .unwrap();
        assert!(limits.to_usage().is_none());
    }
}
