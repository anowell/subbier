//! Provider asymmetries that are *parameters* become the static [`OAuth`]
//! table; ones that are *algorithms* stay as separate functions behind a single
//! `match` ([`fetch_usage_at`] here, `discover` in `auth::discovery`). Nothing
//! downstream of `provider/{codex,claude}.rs` branches on provider.

pub mod claude;
pub mod codex;

use std::time::Duration;

use reqwest::header::HeaderMap;
use tokio::time::Instant;

use crate::error::{Error, Result};
use crate::model::{Credentials, Provider, Usage};

/// How the token endpoint wants its request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BodyKind {
    /// `application/x-www-form-urlencoded` — Codex.
    Form,
    /// `application/json` — Claude.
    Json,
}

/// Where the `state` authorize parameter comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StateSource {
    /// A fresh random value of [`STATE_BYTES`] bytes.
    Random,
    /// The PKCE code verifier itself, sent again as `state`.
    PkceVerifier,
}

/// How long an access token lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExpirySource {
    /// The access token's own JWT `exp` claim; Codex's `expires_in` lies.
    AccessTokenJwtExp,
    /// `now + expires_in` seconds. Claude.
    ExpiresIn,
}

/// Bytes of entropy behind [`StateSource::Random`], before base64url encoding.
pub const STATE_BYTES: usize = 16;

/// Everything the one generic OAuth + PKCE implementation needs to know about
/// a provider. Purely parameters: no behaviour lives here.
#[derive(Debug, Clone, Copy)]
pub struct OAuth {
    pub client_id: &'static str,
    pub authorize_url: &'static str,
    /// Default token endpoint; [`Provider::token_url`] can override it.
    pub token_url: &'static str,
    pub redirect_port: u16,
    pub redirect_path: &'static str,
    /// Scope requested at login.
    pub scope: &'static str,
    /// Scope sent on refresh when it differs from [`OAuth::scope`]. `None`
    /// sends no scope at all.
    pub refresh_scope: Option<&'static str>,
    pub extra_authorize_params: &'static [(&'static str, &'static str)],
    pub token_body: BodyKind,
    pub state: StateSource,
    pub expiry: ExpirySource,
}

impl OAuth {
    /// Must be byte-identical in the authorize request and the token exchange.
    #[must_use]
    pub fn redirect_uri(&self) -> String {
        format!(
            "http://localhost:{}{}",
            self.redirect_port, self.redirect_path
        )
    }
}

pub static CODEX_OAUTH: OAuth = OAuth {
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    authorize_url: "https://auth.openai.com/oauth/authorize",
    token_url: "https://auth.openai.com/oauth/token",
    redirect_port: 1455,
    redirect_path: "/auth/callback",
    scope: "openid profile email offline_access",
    // Refresh deliberately drops `offline_access`.
    refresh_scope: Some("openid profile email"),
    extra_authorize_params: &[
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
    ],
    token_body: BodyKind::Form,
    state: StateSource::Random,
    expiry: ExpirySource::AccessTokenJwtExp,
};

pub static CLAUDE_OAUTH: OAuth = OAuth {
    client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
    authorize_url: "https://claude.ai/oauth/authorize",
    token_url: "https://platform.claude.com/v1/oauth/token",
    redirect_port: 53692,
    redirect_path: "/callback",
    scope: "org:create_api_key user:profile user:inference user:sessions:claude_code \
            user:mcp_servers user:file_upload",
    // Claude reuses the login scope on refresh, so it sends no scope at all.
    refresh_scope: None,
    extra_authorize_params: &[("code", "true")],
    token_body: BodyKind::Json,
    // NOT a separate random value — the server expects the PKCE verifier.
    state: StateSource::PkceVerifier,
    expiry: ExpirySource::ExpiresIn,
};

pub const CODEX_UPSTREAM_BASE: &str = "https://chatgpt.com/backend-api";
pub const CLAUDE_UPSTREAM_BASE: &str = "https://api.anthropic.com";

impl Provider {
    #[must_use]
    pub fn oauth(self) -> &'static OAuth {
        match self {
            Provider::Codex => &CODEX_OAUTH,
            Provider::Claude => &CLAUDE_OAUTH,
        }
    }

    /// The usage endpoint's path below [`Provider::upstream_base`].
    #[must_use]
    pub const fn usage_path(self) -> &'static str {
        match self {
            Provider::Codex => "/wham/usage",
            Provider::Claude => "/api/oauth/usage",
        }
    }

    #[must_use]
    pub fn usage_url_from(self, base: &str) -> String {
        format!("{}{}", base.trim_end_matches('/'), self.usage_path())
    }

    /// The upstream API base. The [`Provider::upstream_base_env`] override
    /// exists so the proxy test harness can aim at a local fake upstream.
    #[must_use]
    pub fn upstream_base(self) -> String {
        let default = match self {
            Provider::Codex => CODEX_UPSTREAM_BASE,
            Provider::Claude => CLAUDE_UPSTREAM_BASE,
        };
        env_override(self.upstream_base_env())
            .map(|v| v.trim_end_matches('/').to_owned())
            .unwrap_or_else(|| default.to_owned())
    }

    /// The token endpoint, overridable by [`Provider::token_url_env`].
    #[must_use]
    pub fn token_url(self) -> String {
        env_override(self.token_url_env()).unwrap_or_else(|| self.oauth().token_url.to_owned())
    }

    #[must_use]
    pub const fn upstream_base_env(self) -> &'static str {
        match self {
            Provider::Codex => "SUBBIER_CODEX_BASE",
            Provider::Claude => "SUBBIER_ANTHROPIC_BASE",
        }
    }

    #[must_use]
    pub const fn token_url_env(self) -> &'static str {
        match self {
            Provider::Codex => "SUBBIER_CODEX_TOKEN_URL",
            Provider::Claude => "SUBBIER_ANTHROPIC_TOKEN_URL",
        }
    }

    /// The env var a *client* exports so `codex` / `claude` talk to subbier —
    /// not one of subbier's own overrides.
    #[must_use]
    pub const fn base_url_env(self) -> &'static str {
        match self {
            Provider::Codex => "OPENAI_BASE_URL",
            Provider::Claude => "ANTHROPIC_BASE_URL",
        }
    }
}

fn env_override(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Fetch and normalise one subscription's usage.
///
/// `deadline` is a round-level deadline shared by every sub in a scoring round,
/// not a per-request timeout.
///
/// A 401 is returned, not refreshed; an `Err` is never an exhausted account.
/// A deadline overrun is recognisable with [`is_deadline_exceeded`].
pub async fn fetch_usage(p: Provider, c: &Credentials, deadline: Instant) -> Result<Usage> {
    fetch_usage_at(p, &p.upstream_base(), c, deadline).await
}

/// [`fetch_usage`] against an explicit upstream base — the test seam.
pub async fn fetch_usage_at(
    p: Provider,
    base: &str,
    c: &Credentials,
    deadline: Instant,
) -> Result<Usage> {
    match p {
        Provider::Codex => codex::fetch_usage(base, c, deadline).await,
        Provider::Claude => claude::fetch_usage(base, c, deadline).await,
    }
}

/// Matched by [`is_deadline_exceeded`]; do not reword one without the other.
const DEADLINE_MESSAGE: &str = "usage fetch exceeded the round-level deadline";

#[must_use]
pub fn deadline_exceeded() -> Error {
    Error::other(DEADLINE_MESSAGE)
}

/// Whether an error means "we ran out of time" rather than "the provider said
/// no". A slow account must never look like an exhausted one.
#[must_use]
pub fn is_deadline_exceeded(e: &Error) -> bool {
    match e {
        Error::Other(m) => m == DEADLINE_MESSAGE,
        Error::Http(h) => h.is_timeout(),
        _ => false,
    }
}

/// Whether an error is a 401, i.e. "refresh the token and try again".
#[must_use]
pub fn is_unauthorized(e: &Error) -> bool {
    matches!(e, Error::Upstream { status: 401, .. })
}

/// How much of a response body an [`Error::Upstream`] may quote.
const BODY_EXCERPT: usize = 200;

/// GET `url` with `headers`. Both the reqwest timeout and an outer `timeout_at`
/// apply: only the latter also covers reading the body.
pub(crate) async fn get_text(url: &str, headers: HeaderMap, deadline: Instant) -> Result<String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(deadline_exceeded());
    }
    let request = crate::http::client()
        .get(url)
        .headers(headers)
        .timeout(remaining);

    let fetch = async move {
        let response = request.send().await?;
        let status = response.status();
        let response_headers = response.headers().clone();
        let body = response.text().await?;
        if let Some(err) = challenge_error(status.as_u16(), &response_headers, &body) {
            return Err(err);
        }
        if !status.is_success() {
            return Err(Error::upstream_after(
                status.as_u16(),
                excerpt(&body),
                retry_after(&response_headers),
            ));
        }
        Ok(body)
    };

    match tokio::time::timeout_at(deadline, fetch).await {
        Ok(result) => result,
        Err(_) => Err(deadline_exceeded()),
    }
}

/// The `Retry-After` header, when it names a positive whole number of seconds.
///
/// Anthropic answers a 429 with `retry-after: 0`, and obeying that literally kept
/// a rate limit alive for a quarter of an hour. HTTP-date form is not parsed.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

/// A Cloudflare challenge, reported as a distinct upstream error rather than
/// surfacing later as an inscrutable JSON parse failure.
fn challenge_error(status: u16, headers: &HeaderMap, body: &str) -> Option<Error> {
    let mitigated = headers.contains_key("cf-mitigated");
    let html = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));
    if mitigated || (html && !status_ok(status)) {
        let why = if mitigated {
            "challenged by Cloudflare (cf-mitigated)"
        } else {
            "challenged by Cloudflare (HTML response)"
        };
        return Some(Error::upstream(status, format!("{why}: {}", excerpt(body))));
    }
    None
}

const fn status_ok(status: u16) -> bool {
    status >= 200 && status < 300
}

/// At most [`BODY_EXCERPT`] chars of a body, on one line.
fn excerpt(body: &str) -> String {
    let flattened: String = body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(BODY_EXCERPT)
        .collect();
    let trimmed = flattened.trim();
    if body.len() > BODY_EXCERPT {
        format!("{trimmed}…")
    } else {
        trimmed.to_owned()
    }
}

/// Missing or non-finite is `0`, clamped to the `0..=100` that
/// [`crate::model::UsageWindow::pct`] promises.
pub(crate) fn normalise_pct(raw: Option<f32>) -> f32 {
    raw.filter(|p| p.is_finite())
        .unwrap_or(0.0)
        .clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_keeps_positive_seconds_and_drops_everything_else() {
        let header = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("retry-after", value.parse().unwrap());
            retry_after(&headers)
        };

        assert_eq!(header("30"), Some(Duration::from_secs(30)));
        assert_eq!(header(" 30 "), Some(Duration::from_secs(30)));
        // What the Anthropic usage endpoint really sends with its 429.
        assert_eq!(header("0"), None);
        assert_eq!(header("-5"), None);
        assert_eq!(header("Wed, 27 Aug 2026 12:00:00 GMT"), None);
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn usage_urls_compose_from_a_base() {
        assert_eq!(
            Provider::Codex.usage_url_from("http://127.0.0.1:9/backend-api/"),
            "http://127.0.0.1:9/backend-api/wham/usage"
        );
        assert_eq!(
            Provider::Claude.usage_url_from(CLAUDE_UPSTREAM_BASE),
            "https://api.anthropic.com/api/oauth/usage"
        );
        assert_eq!(
            Provider::Codex.usage_url_from(CODEX_UPSTREAM_BASE),
            "https://chatgpt.com/backend-api/wham/usage"
        );
    }

    #[test]
    fn a_missing_percentage_is_zero_and_a_wild_one_is_clamped() {
        assert_eq!(normalise_pct(None), 0.0);
        assert_eq!(normalise_pct(Some(f32::NAN)), 0.0);
        assert_eq!(normalise_pct(Some(-3.0)), 0.0);
        assert_eq!(normalise_pct(Some(140.0)), 100.0);
        assert_eq!(normalise_pct(Some(41.5)), 41.5);
    }

    /// Codex sends epoch seconds and Claude an ISO string; the normalised values must not differ.
    #[test]
    fn epoch_seconds_and_an_iso_string_land_on_the_same_timestamp() {
        let from_codex = codex::parse_usage(
            r#"{"rate_limit":{"primary_window":{"limit_window_seconds":18000,"reset_at":1787798509}}}"#,
        )
        .unwrap();
        let from_claude = claude::parse_usage(
            r#"{"limits":[{"kind":"session","group":"session","resets_at":"2026-08-27T02:41:49+00:00"}]}"#,
        )
        .unwrap();

        let codex_session = from_codex.session.expect("codex session window");
        let claude_session = from_claude.session.expect("claude session window");
        assert_eq!(codex_session.resets_at, claude_session.resets_at);
        assert_eq!(
            codex_session.resets_at,
            jiff::Timestamp::from_second(1_787_798_509).ok()
        );
        // The 5h width agrees too: exact on one side, nominal on the other.
        assert_eq!(codex_session.started_at, claude_session.started_at);
    }

    #[test]
    fn deadline_and_unauthorized_errors_are_distinguishable() {
        assert!(is_deadline_exceeded(&deadline_exceeded()));
        assert!(!is_deadline_exceeded(&Error::upstream(401, "nope")));
        assert!(is_unauthorized(&Error::upstream(401, "nope")));
        assert!(!is_unauthorized(&Error::upstream(500, "nope")));
        assert!(!is_unauthorized(&deadline_exceeded()));
    }

    #[test]
    fn body_excerpts_are_short_and_single_line() {
        assert_eq!(excerpt("  {\"error\":\n\"x\"}  "), "{\"error\": \"x\"}");
        let long = "a".repeat(BODY_EXCERPT * 2);
        let cut = excerpt(&long);
        assert!(cut.ends_with('…'));
        assert_eq!(cut.chars().count(), BODY_EXCERPT + 1);
    }
}
