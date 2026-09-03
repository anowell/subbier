//! Codex usage parsing. Every Codex unit quirk dies here; downstream sees only
//! [`crate::model::Usage`].

use jiff::{SignedDuration, Timestamp};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::Value;
use tokio::time::Instant;

use super::{get_text, normalise_pct};
use crate::error::{Error, Result};
use crate::model::{Credentials, Provider, Usage, UsageWindow};

/// A window at most this wide is the short "session" window; anything wider —
/// or unstated — is the weekly one. The `None => Weekly` half matches the
/// reference implementation's `w.limit_window_seconds <= 100_000`, which in JS
/// is `false` for `undefined`.
pub const SESSION_WINDOW_MAX_SECONDS: i64 = 100_000;

/// Sent as an empty string when we have no account id, never omitted.
const ACCOUNT_ID_HEADER: &str = "chatgpt-account-id";

/// `GET {base}/wham/usage`, normalised. Takes already-fresh credentials: a 401
/// comes back for the caller to refresh and retry.
pub async fn fetch_usage(base: &str, c: &Credentials, deadline: Instant) -> Result<Usage> {
    let url = Provider::Codex.usage_url_from(base);
    let body = get_text(&url, headers(c)?, deadline).await?;
    parse_usage(&body)
}

fn headers(c: &Credentials) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    let mut bearer = HeaderValue::from_str(&format!("Bearer {}", c.tokens.access))
        .map_err(|_| Error::auth("codex access token is not a valid header value"))?;
    bearer.set_sensitive(true);
    headers.insert(AUTHORIZATION, bearer);
    let account = c.account_id.as_deref().unwrap_or("");
    headers.insert(
        ACCOUNT_ID_HEADER,
        HeaderValue::from_str(account)
            .map_err(|_| Error::auth("codex account id is not a valid header value"))?,
    );
    Ok(headers)
}

/// Parse a `/wham/usage` body.
///
/// Unknown fields are ignored, so upstream additions do not break the parse.
pub fn parse_usage(body: &str) -> Result<Usage> {
    let raw: RawUsage = serde_json::from_str(body)?;
    Ok(raw.normalise(Timestamp::now()))
}

#[derive(Debug, Default, Deserialize)]
struct RawUsage {
    plan_type: Option<String>,
    rate_limit: Option<RawRateLimit>,
    /// Present at the top level as well as inside `rate_limit`.
    additional_rate_limits: Option<Value>,
}

impl RawUsage {
    fn normalise(self, now: Timestamp) -> Usage {
        let mut usage = self
            .rate_limit
            .map(|r| r.normalise(now))
            .unwrap_or_default();
        usage.plan = self.plan_type;
        if usage.scoped.is_empty() {
            usage.scoped = scoped_windows(self.additional_rate_limits.as_ref(), now);
        }
        usage
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawRateLimit {
    /// Codex's explicit verdict. `null` on bodies that predate it.
    limit_reached: Option<bool>,
    /// The same verdict inverted; used only when `limit_reached` is absent.
    allowed: Option<bool>,
    primary_window: Option<RawWindow>,
    secondary_window: Option<RawWindow>,
    additional_rate_limits: Option<Value>,
}

impl RawRateLimit {
    fn normalise(self, now: Timestamp) -> Usage {
        let mut usage = Usage::default();
        // Same order as the reference implementation, so that two windows of
        // the same class resolve to the same winner.
        let windows = [self.primary_window, self.secondary_window];
        for raw in windows.into_iter().flatten() {
            let window = raw.normalise(now);
            if raw.is_session() {
                usage.session = Some(window);
            } else {
                usage.weekly = Some(window);
            }
        }
        usage.scoped = scoped_windows(self.additional_rate_limits.as_ref(), now);
        // `None` when the body stated neither field, which is not `Some(false)`.
        usage.limit_reached = self.limit_reached.or_else(|| self.allowed.map(|a| !a));
        usage
    }
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
struct RawWindow {
    used_percent: Option<f32>,
    limit_window_seconds: Option<i64>,
    /// Absolute reset, epoch seconds (contrast Claude's ISO string).
    reset_at: Option<i64>,
    /// Relative reset, seconds from now.
    reset_after_seconds: Option<i64>,
}

impl RawWindow {
    fn width_seconds(&self) -> Option<i64> {
        self.limit_window_seconds.filter(|&s| s > 0)
    }

    /// A width we were not told is weekly.
    fn is_session(&self) -> bool {
        matches!(self.width_seconds(), Some(s) if s <= SESSION_WINDOW_MAX_SECONDS)
    }

    fn resets_at(&self, now: Timestamp) -> Option<Timestamp> {
        if let Some(seconds) = self.reset_at.filter(|&s| s > 0) {
            return Timestamp::from_second(seconds).ok();
        }
        let relative = self.reset_after_seconds.filter(|&s| s > 0)?;
        now.checked_add(SignedDuration::from_secs(relative)).ok()
    }

    fn normalise(&self, now: Timestamp) -> UsageWindow {
        let resets_at = self.resets_at(now);
        // Exact for Codex: the API states both the reset and the width.
        let started_at = match (resets_at, self.width_seconds()) {
            (Some(reset), Some(width)) => reset.checked_sub(SignedDuration::from_secs(width)).ok(),
            _ => None,
        };
        UsageWindow {
            pct: normalise_pct(self.used_percent),
            resets_at,
            started_at,
        }
    }
}

/// `additional_rate_limits` as scoped windows. The field is `null` on every
/// live response captured so far, so both shapes are accepted: an array of
/// self-naming objects, or a map of name to window. An unnamed entry is skipped
/// rather than given an invented name.
fn scoped_windows(value: Option<&Value>, now: Timestamp) -> Vec<(String, UsageWindow)> {
    let mut out = Vec::new();
    match value {
        Some(Value::Array(entries)) => {
            for entry in entries {
                if let (Some(name), Ok(raw)) = (
                    entry_name(entry),
                    serde_json::from_value::<RawWindow>(entry.clone()),
                ) {
                    out.push((name, raw.normalise(now)));
                }
            }
        }
        Some(Value::Object(entries)) => {
            for (name, entry) in entries {
                if let Ok(raw) = serde_json::from_value::<RawWindow>(entry.clone()) {
                    out.push((name.to_lowercase(), raw.normalise(now)));
                }
            }
        }
        _ => {}
    }
    out
}

/// The name an `additional_rate_limits` entry gives itself, lowercased.
fn entry_name(entry: &Value) -> Option<String> {
    ["name", "kind", "limit_type", "window_type", "slug", "id"]
        .iter()
        .find_map(|key| entry.get(key).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real, redacted `/wham/usage` body.
    const LIVE_BODY: &str = include_str!("fixtures/codex_usage.json");

    /// The same body with the account cut off, and a primary window below 100%.
    const LIMIT_REACHED_BODY: &str = include_str!("fixtures/codex_usage_limit_reached.json");

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(seconds).unwrap()
    }

    #[test]
    fn the_live_verified_body_parses() {
        let usage = parse_usage(LIVE_BODY).unwrap();
        assert_eq!(usage.plan.as_deref(), Some("plus"));

        let session = usage.session.expect("session window");
        assert_eq!(session.pct, 0.0);
        assert_eq!(session.resets_at, Some(at(1_787_798_509)));
        // Exact: reset_at - limit_window_seconds.
        assert_eq!(session.started_at, Some(at(1_787_798_509 - 18_000)));

        let weekly = usage.weekly.expect("weekly window");
        assert_eq!(weekly.pct, 16.0);
        assert_eq!(weekly.resets_at, Some(at(1_788_290_706)));
        assert_eq!(weekly.started_at, Some(at(1_788_290_706 - 604_800)));

        assert!(usage.scoped.is_empty());
        assert_eq!(usage.limit_reached, Some(false));
    }

    #[test]
    fn the_explicit_limit_reached_watermark_survives_a_sub_100_percentage() {
        let usage = parse_usage(LIMIT_REACHED_BODY).unwrap();
        assert_eq!(usage.limit_reached, Some(true));
        // Below 100: the endpoint has not caught up with the enforcement
        // decision, and the flag is what tells us.
        assert_eq!(usage.session.unwrap().pct, 87.0);
        assert_eq!(usage.weekly.unwrap().pct, 16.0);
    }

    #[test]
    fn limit_reached_falls_back_to_allowed_and_stays_none_when_unstated() {
        let verdict = |body: &str| parse_usage(body).unwrap().limit_reached;

        assert_eq!(
            verdict(r#"{"rate_limit":{"allowed":false,"primary_window":{}}}"#),
            Some(true)
        );
        assert_eq!(
            verdict(r#"{"rate_limit":{"allowed":true,"primary_window":{}}}"#),
            Some(false)
        );
        // `limit_reached` wins over `allowed` when the body states both.
        assert_eq!(
            verdict(r#"{"rate_limit":{"allowed":true,"limit_reached":true}}"#),
            Some(true)
        );
        // `None` is "the provider did not say", never `Some(false)`.
        assert_eq!(
            verdict(r#"{"rate_limit":{"primary_window":{"used_percent":7}}}"#),
            None
        );
        assert_eq!(verdict(r#"{"plan_type":"pro","rate_limit":null}"#), None);
    }

    #[test]
    fn window_width_classifies_session_versus_weekly() {
        let window = |width: String| {
            parse_usage(&format!(
                r#"{{"rate_limit":{{"primary_window":{{"used_percent":7,"reset_at":1787798509{width}}}}}}}"#
            ))
            .unwrap()
        };

        let exactly_at_threshold = window(format!(
            ",\"limit_window_seconds\":{SESSION_WINDOW_MAX_SECONDS}"
        ));
        assert!(exactly_at_threshold.session.is_some());
        let over = window(format!(
            ",\"limit_window_seconds\":{}",
            SESSION_WINDOW_MAX_SECONDS + 1
        ));
        assert!(over.weekly.is_some());

        // An unstated width is weekly, and gives no derivable start.
        let unstated = window(String::new());
        assert!(unstated.session.is_none(), "must not classify as session");
        let weekly = unstated.weekly.expect("weekly window");
        assert_eq!(weekly.pct, 7.0);
        assert_eq!(weekly.started_at, None);
    }

    #[test]
    fn absent_fields_default_rather_than_fail() {
        let usage =
            parse_usage(r#"{"rate_limit":{"primary_window":{"limit_window_seconds":18000}}}"#)
                .unwrap();
        let session = usage.session.unwrap();
        assert_eq!(session.pct, 0.0);
        assert_eq!(session.resets_at, None);
        assert_eq!(session.started_at, None);
    }

    #[test]
    fn a_body_with_no_rate_limit_is_not_an_error() {
        let usage = parse_usage(r#"{"plan_type":"pro","rate_limit":null}"#).unwrap();
        assert_eq!(usage.plan.as_deref(), Some("pro"));
        assert_eq!(usage.session, None);
        assert_eq!(usage.weekly, None);
    }

    #[test]
    fn additional_rate_limits_become_scoped_windows() {
        let usage = parse_usage(
            r#"{"rate_limit":{"primary_window":{"limit_window_seconds":18000,"reset_at":1787798509},
               "additional_rate_limits":[
                 {"name":"Code review","used_percent":12,"limit_window_seconds":604800,"reset_at":1788290706},
                 {"used_percent":99}]}}"#,
        )
        .unwrap();
        // The unnamed entry is skipped rather than given an invented name.
        assert_eq!(usage.scoped.len(), 1);
        let (name, window) = &usage.scoped[0];
        assert_eq!(name, "code review");
        assert_eq!(window.pct, 12.0);
        assert_eq!(window.started_at, Some(at(1_788_290_706 - 604_800)));
    }

    #[test]
    fn the_account_id_header_is_empty_rather_than_absent() {
        let credentials = crate::model::Credentials {
            plan: None,
            account_id: None,
            email: None,
            tokens: crate::model::Tokens {
                access: "tok".into(),
                refresh: None,
                expires_at: None,
            },
            source: crate::model::CredentialSource::Keychain,
        };
        let headers = headers(&credentials).unwrap();
        assert_eq!(headers[ACCOUNT_ID_HEADER], "");
        assert!(headers[AUTHORIZATION].is_sensitive());
    }
}
