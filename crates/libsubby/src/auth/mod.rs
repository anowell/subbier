//! One OAuth flow, one refresh path, one dedupe map.
//!
//! Every asymmetry between Codex and Claude is a lookup in the static
//! [`crate::provider::OAuth`] table, never an `if provider == …` here.

pub mod discovery;
pub mod pkce;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use jiff::{SignedDuration, Timestamp};
use reqwest::RequestBuilder;
use serde_json::Value;
use tokio::sync::OnceCell;
use tracing::Instrument as _;

use crate::error::{Error, Result};
use crate::model::{CredentialSource, Credentials, Provider, Sub, SubKey, Tokens};
use crate::provider::{BodyKind, ExpirySource, STATE_BYTES, StateSource};

use self::pkce::{CodeWaiter, Pkce};

/// How far ahead of the real expiry a token counts as stale.
pub const EXPIRY_SKEW: SignedDuration = SignedDuration::from_secs(60);

/// Timeout for one call to a token endpoint. Overrunning it is transient.
pub const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The account component of a [`SubKey`] when the provider named no identity.
pub const UNKNOWN_ACCOUNT: &str = "default";

/// Why a token refresh failed; only a permanent failure should quarantine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshError {
    /// Never carries a token, so it is safe to log.
    pub message: String,
    /// `true` only for 400/401/403 from the token endpoint.
    pub permanent: bool,
}

impl RefreshError {
    #[must_use]
    pub fn permanent(message: impl fmt::Display) -> Self {
        Self {
            message: message.to_string(),
            permanent: true,
        }
    }

    #[must_use]
    pub fn transient(message: impl fmt::Display) -> Self {
        Self {
            message: message.to_string(),
            permanent: false,
        }
    }

    #[must_use]
    pub const fn outcome(&self) -> &'static str {
        if self.permanent {
            "permanent"
        } else {
            "transient"
        }
    }
}

impl fmt::Display for RefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RefreshError {}

impl From<RefreshError> for Error {
    fn from(e: RefreshError) -> Self {
        Error::Auth(e.message)
    }
}

/// The claims of a JWT, **without verifying its signature** — it arrived from
/// the issuer over TLS.
#[must_use]
pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The `exp` claim of an access token; Codex's `expires_in` is not to be trusted.
#[must_use]
pub fn access_token_expiry(token: &str) -> Option<Timestamp> {
    let exp = jwt_claims(token)?.get("exp")?.as_i64()?;
    Timestamp::from_second(exp).ok()
}

#[derive(Debug, Clone)]
pub struct AuthorizeRequest {
    pub url: String,
    /// For Claude this **is** the PKCE verifier — that looks like a bug and is not.
    pub state: String,
    pub pkce: Pkce,
}

#[must_use]
pub fn authorize_request(p: Provider) -> AuthorizeRequest {
    let oauth = p.oauth();
    let pkce = Pkce::generate();
    let state = match oauth.state {
        StateSource::Random => pkce::random_urlsafe(STATE_BYTES),
        StateSource::PkceVerifier => pkce.verifier().to_owned(),
    };

    let mut url = url::Url::parse(oauth.authorize_url).expect("authorize_url is a static constant");
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", oauth.client_id)
            .append_pair("redirect_uri", &oauth.redirect_uri())
            .append_pair("scope", oauth.scope)
            .append_pair("code_challenge", pkce.challenge())
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);
        for (key, value) in oauth.extra_authorize_params {
            query.append_pair(key, value);
        }
    }

    AuthorizeRequest {
        url: url.into(),
        state,
        pkce,
    }
}

/// Run the full browser OAuth + PKCE flow for `p`.
///
/// `on_url` is called only after the loopback listener is bound, so a port
/// conflict surfaces before the user reaches a dead redirect. Dropping the
/// returned future cancels the flow and frees the port.
pub async fn login(p: Provider, on_url: impl FnOnce(&str)) -> Result<Credentials> {
    let oauth = p.oauth();
    let request = authorize_request(p);

    let mut waiter = CodeWaiter::bind(oauth.redirect_port, oauth.redirect_path).await?;
    on_url(&request.url);

    let callback = waiter.recv().await?;
    // An empty echo means the provider sent no state, which we cannot check.
    if !callback.state.is_empty() && callback.state != request.state {
        return Err(Error::auth(
            "the sign-in callback carried the wrong state parameter",
        ));
    }

    let response = exchange_code_at(
        p,
        &p.token_url(),
        &callback.code,
        request.pkce.verifier(),
        &request.state,
    )
    .await?;

    let tokens = tokens_from(p, &response, None)?;
    Ok(Credentials {
        account_id: account_id_from(&response, &tokens.access),
        email: email_from(&response, &tokens.access),
        plan: None,
        tokens,
        source: CredentialSource::Subbier,
    })
}

/// Open `url` in the user's browser.
pub fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = std::process::Command::new("xdg-open");

    command
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

/// Exchange a refresh token for a fresh access token. The returned [`Tokens`]
/// keeps the refresh token passed in unless the provider rotated it.
/// Check `RefreshError::permanent` before quarantining anything.
pub async fn refresh(p: Provider, refresh_token: &str) -> Result<Tokens, RefreshError> {
    refresh_at(p, &p.token_url(), refresh_token).await
}

/// [`refresh`] against an explicit token endpoint — the test seam.
pub async fn refresh_at(
    p: Provider,
    token_url: &str,
    refresh_token: &str,
) -> Result<Tokens, RefreshError> {
    let oauth = p.oauth();
    let mut params = vec![
        ("grant_type", "refresh_token".to_owned()),
        ("refresh_token", refresh_token.to_owned()),
        ("client_id", oauth.client_id.to_owned()),
    ];
    // Codex drops `offline_access` on refresh; Claude sends no scope at all.
    if let Some(scope) = oauth.refresh_scope {
        params.push(("scope", scope.to_owned()));
    }

    let response = post_token(p, token_url, params).await?;
    tokens_from(p, &response, Some(refresh_token)).map_err(RefreshError::transient)
}

async fn exchange_code_at(
    p: Provider,
    token_url: &str,
    code: &str,
    verifier: &str,
    state: &str,
) -> Result<Value> {
    let oauth = p.oauth();
    let mut params = vec![
        ("grant_type", "authorization_code".to_owned()),
        ("code", code.to_owned()),
        ("redirect_uri", oauth.redirect_uri()),
        ("client_id", oauth.client_id.to_owned()),
        ("code_verifier", verifier.to_owned()),
    ];
    // A provider whose `state` *is* the verifier expects it echoed here too.
    if oauth.state == StateSource::PkceVerifier {
        params.push(("state", state.to_owned()));
    }

    post_token(p, token_url, params)
        .await
        .map_err(|e| Error::auth(format!("token exchange failed: {e}")))
}

async fn post_token(
    p: Provider,
    token_url: &str,
    params: Vec<(&'static str, String)>,
) -> Result<Value, RefreshError> {
    let request = crate::http::client()
        .post(token_url)
        .timeout(TOKEN_REQUEST_TIMEOUT);
    let request = encode_body(request, p.oauth().token_body, &params);

    let response = request
        .send()
        .await
        .map_err(|e| RefreshError::transient(format!("token request failed: {e}")))?;

    let status = response.status().as_u16();
    // A challenge never reached the token endpoint, so it is never permanent.
    let challenged = response.headers().contains_key("cf-mitigated")
        || response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/html"));

    let body = response
        .text()
        .await
        .map_err(|e| RefreshError::transient(format!("reading the token response failed: {e}")))?;

    if !(200..300).contains(&status) {
        let permanent = !challenged && matches!(status, 400 | 401 | 403);
        let detail = error_code(&body).map_or_else(String::new, |code| format!(" ({code})"));
        let challenge = if challenged {
            " (Cloudflare challenge)"
        } else {
            ""
        };
        return Err(RefreshError {
            message: format!("token endpoint returned {status}{detail}{challenge}"),
            permanent,
        });
    }

    serde_json::from_str(&body)
        .map_err(|_| RefreshError::transient("the token endpoint returned a malformed response"))
}

fn encode_body(
    request: RequestBuilder,
    kind: BodyKind,
    params: &[(&'static str, String)],
) -> RequestBuilder {
    match kind {
        BodyKind::Form => request.form(params),
        BodyKind::Json => {
            let object: serde_json::Map<String, Value> = params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), Value::String(v.clone())))
                .collect();
            request.json(&Value::Object(object))
        }
    }
}

/// The OAuth `error` code out of an error body; the rest of the body is
/// discarded rather than risk quoting a secret.
fn error_code(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let code = value.get("error")?;
    let code = code.as_str().or_else(|| code.get("type")?.as_str())?;
    (code.len() <= 64).then(|| code.to_owned())
}

fn tokens_from(p: Provider, response: &Value, previous_refresh: Option<&str>) -> Result<Tokens> {
    let access = response
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::auth("the token endpoint returned no access_token"))?
        .to_owned();

    let refresh = response
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| previous_refresh.map(str::to_owned));

    let expires_at = match p.oauth().expiry {
        ExpirySource::AccessTokenJwtExp => access_token_expiry(&access),
        ExpirySource::ExpiresIn => {
            response
                .get("expires_in")
                .and_then(Value::as_i64)
                .and_then(|s| {
                    Timestamp::now()
                        .checked_add(SignedDuration::from_secs(s))
                        .ok()
                })
        }
    };

    Ok(Tokens {
        access,
        refresh,
        expires_at,
    })
}

/// Codex mints the account id into the id token; Claude answers with an
/// `organization`.
fn account_id_from(response: &Value, access: &str) -> Option<String> {
    let id_token = response.get("id_token").and_then(Value::as_str);
    let claims = id_token.and_then(jwt_claims);
    claims
        .as_ref()
        .and_then(|c| {
            c.get("https://api.openai.com/auth")?
                .get("chatgpt_account_id")?
                .as_str()
                .map(str::to_owned)
        })
        .or_else(|| string_at(response, &["organization", "uuid"]))
        .or_else(|| string_at(response, &["account", "uuid"]))
        .or_else(|| {
            jwt_claims(access)?
                .get("https://api.openai.com/auth")?
                .get("chatgpt_account_id")?
                .as_str()
                .map(str::to_owned)
        })
}

fn email_from(response: &Value, access: &str) -> Option<String> {
    let claims = response
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(jwt_claims);
    claims
        .as_ref()
        .and_then(|c| c.get("email")?.as_str().map(str::to_owned))
        .or_else(|| string_at(response, &["account", "email_address"]))
        .or_else(|| string_at(response, &["account", "email"]))
        .or_else(|| {
            jwt_claims(access)?
                .get("email")?
                .as_str()
                .map(str::to_owned)
        })
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(key)?;
    }
    cursor.as_str().map(str::to_owned)
}

/// Account id, else email, else [`UNKNOWN_ACCOUNT`].
#[must_use]
pub fn account_key(p: Provider, c: &Credentials) -> SubKey {
    let account = c
        .account_id
        .as_deref()
        .or(c.email.as_deref())
        .unwrap_or(UNKNOWN_ACCOUNT);
    SubKey::new(p, account)
}

/// Email, else account id, else the provider's display name.
#[must_use]
pub fn default_label(p: Provider, c: &Credentials) -> String {
    c.email
        .clone()
        .or_else(|| c.account_id.clone())
        .unwrap_or_else(|| p.display_name().to_owned())
}

#[must_use]
pub fn to_sub(p: Provider, credentials: Credentials) -> Sub {
    Sub {
        key: account_key(p, &credentials),
        provider: p,
        label: default_label(p, &credentials),
        credentials,
    }
}

/// Per-provider token endpoints — the test seam for a fake endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenUrls {
    pub codex: String,
    pub claude: String,
}

impl TokenUrls {
    /// The real endpoints, honouring `SUBBIER_CODEX_TOKEN_URL` /
    /// `SUBBIER_ANTHROPIC_TOKEN_URL`.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            codex: Provider::Codex.token_url(),
            claude: Provider::Claude.token_url(),
        }
    }

    /// One URL for both providers.
    #[must_use]
    pub fn all(url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            codex: url.clone(),
            claude: url,
        }
    }

    #[must_use]
    pub fn get(&self, p: Provider) -> &str {
        match p {
            Provider::Codex => &self.codex,
            Provider::Claude => &self.claude,
        }
    }
}

impl Default for TokenUrls {
    fn default() -> Self {
        Self::from_env()
    }
}

/// The outcome of one refresh, shared by every caller that deduped onto it.
type Shared = Arc<OnceCell<Result<Tokens, RefreshError>>>;

/// Keeps access tokens fresh, with **one in-flight refresh per sub**: the
/// first caller drives it, the rest await the same result.
#[derive(Debug, Default)]
pub struct TokenManager {
    urls: TokenUrls,
    inflight: Mutex<HashMap<SubKey, Shared>>,
}

impl TokenManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A manager pointed at explicit token endpoints — the test seam.
    #[must_use]
    pub fn with_token_urls(urls: TokenUrls) -> Self {
        Self {
            urls,
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// Refresh `sub`'s tokens if they expire within [`EXPIRY_SKEW`], or if
    /// `force`. Returns whether the access token **changed** — the proxy's 401
    /// retry forces a refresh only when the token did not change under it.
    ///
    /// Quarantine the account only on a permanent [`RefreshError`].
    pub async fn ensure_fresh(&self, sub: &mut Sub, force: bool) -> Result<bool, RefreshError> {
        if !force
            && !sub
                .credentials
                .tokens
                .is_expired(Timestamp::now(), EXPIRY_SKEW)
        {
            return Ok(false);
        }

        let Some(refresh_token) = sub.credentials.tokens.refresh.clone() else {
            return Err(RefreshError::permanent(format!(
                "{} has no refresh token; sign in again",
                sub.key
            )));
        };

        let key = sub.key.clone();
        let (cell, deduped) = self.cell_for(&key);
        let provider = sub.provider;
        let url = self.urls.get(provider).to_owned();
        // `recover_adopted` needs the tokens we are about to overwrite.
        let before = sub.clone();

        let span = tracing::info_span!(
            "auth.refresh",
            sub = %key,
            provider = %provider,
            forced = force,
            deduped,
            outcome = tracing::field::Empty,
        );

        let outcome = cell
            .get_or_init(|| async {
                match refresh_at(provider, &url, &refresh_token).await {
                    // Inside the dedupe cell: a burst of callers re-reads once.
                    Err(e) if e.permanent => recover_adopted(&before, &url).await.unwrap_or(Err(e)),
                    other => other,
                }
            })
            .instrument(span.clone())
            .await
            .clone();

        self.retire(&key, &cell);

        match outcome {
            Ok(tokens) => {
                let changed = tokens.access != sub.credentials.tokens.access;
                sub.credentials.tokens = tokens;
                span.record("outcome", if changed { "refreshed" } else { "unchanged" });
                Ok(changed)
            }
            Err(e) => {
                span.record("outcome", e.outcome());
                tracing::warn!(sub = %key, provider = %provider, permanent = e.permanent, error = %e, "token refresh failed");
                Err(e)
            }
        }
    }

    /// The shared cell for `key`, and whether we joined an existing refresh.
    fn cell_for(&self, key: &SubKey) -> (Shared, bool) {
        let mut inflight = self.inflight.lock().expect("in-flight refresh map");
        match inflight.get(key) {
            Some(cell) => (cell.clone(), true),
            None => {
                let cell: Shared = Arc::new(OnceCell::new());
                inflight.insert(key.clone(), cell.clone());
                (cell, false)
            }
        }
    }

    /// Drop the map entry only if it is still the one we awaited, so a
    /// straggler cannot evict a later refresh.
    fn retire(&self, key: &SubKey, cell: &Shared) {
        let mut inflight = self.inflight.lock().expect("in-flight refresh map");
        if inflight
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, cell))
        {
            inflight.remove(key);
        }
    }
}

/// Recover an **adopted** credential whose refresh token the vendor rotated:
/// our snapshot is dead but the source may already hold a live one. `None`
/// means there is nothing to recover and `needs login` is the truth.
async fn recover_adopted(sub: &Sub, token_url: &str) -> Option<Result<Tokens, RefreshError>> {
    if matches!(sub.credentials.source, CredentialSource::Subbier) {
        return None;
    }
    let key = sub.key.clone();
    let current = sub.credentials.tokens.clone();

    // A file read and, on macOS, a `security` subprocess.
    let owned = sub.clone();
    let found = tokio::task::spawn_blocking(move || discovery::reread(&owned))
        .await
        .inspect_err(|e| tracing::debug!(sub = %key, error = %e, "re-reading the source panicked"))
        .ok()
        .flatten()?;

    if found.tokens.refresh == current.refresh && found.tokens.access == current.access {
        tracing::debug!(sub = %key, "the adopted source still holds the same dead credential");
        return None;
    }

    // Refreshing a credential the vendor just refreshed would rotate its token
    // out from under it, starting the same race in the other direction.
    if !found.tokens.is_expired(Timestamp::now(), EXPIRY_SKEW) {
        tracing::info!(sub = %key, "adopted a newer credential from the source; no refresh needed");
        return Some(Ok(found.tokens));
    }

    let refresh_token = found.tokens.refresh.clone()?;
    tracing::info!(sub = %key, "adopted a newer refresh token from the source; retrying once");
    Some(refresh_at(sub.provider, token_url, &refresh_token).await)
}

// The engine shares one TokenManager across every task that touches a sub.
const _: fn() = || {
    fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<TokenManager>();
    assert_send_sync_static::<RefreshError>();
};

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;

    use super::*;

    /// What the fake token endpoint recorded about the last request.
    #[derive(Debug, Default)]
    struct Seen {
        hits: AtomicUsize,
        content_type: Mutex<String>,
        body: Mutex<String>,
    }

    fn jwt(claims: &Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        format!("{header}.{payload}.")
    }

    fn codex_access_token(exp: i64) -> String {
        jwt(&serde_json::json!({
            "exp": exp,
            "email": "me@example.com",
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct-1" },
        }))
    }

    async fn fake_token_endpoint(
        status: StatusCode,
        body: Value,
        delay: Duration,
    ) -> (String, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        let state = (seen.clone(), status, body, delay);
        let app = Router::new()
            .route(
                "/token",
                post(
                    move |State((seen, status, body, delay)): State<(
                        Arc<Seen>,
                        StatusCode,
                        Value,
                        Duration,
                    )>,
                          headers: HeaderMap,
                          raw: String| async move {
                        seen.hits.fetch_add(1, Ordering::SeqCst);
                        *seen.content_type.lock().unwrap() = headers
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        *seen.body.lock().unwrap() = raw;
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        (status, axum::Json(body))
                    },
                ),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (url, seen)
    }

    fn sub(provider: Provider, expires_at: Option<Timestamp>) -> Sub {
        let credentials = Credentials {
            plan: None,
            account_id: Some("acct-1".into()),
            email: None,
            tokens: Tokens {
                access: "stale-access".into(),
                refresh: Some("refresh-1".into()),
                expires_at,
            },
            source: CredentialSource::Subbier,
        };
        to_sub(provider, credentials)
    }

    #[test]
    fn claude_sends_the_pkce_verifier_as_state_and_codex_does_not() {
        let claude = authorize_request(Provider::Claude);
        assert_eq!(claude.state, claude.pkce.verifier());

        let codex = authorize_request(Provider::Codex);
        assert_ne!(codex.state, codex.pkce.verifier());
        assert!(!codex.state.is_empty());
    }

    #[test]
    fn authorize_urls_carry_the_table_verbatim() {
        for provider in Provider::ALL {
            let oauth = provider.oauth();
            let request = authorize_request(provider);
            let url = url::Url::parse(&request.url).unwrap();
            let query: HashMap<_, _> = url.query_pairs().into_owned().collect();

            assert!(request.url.starts_with(oauth.authorize_url));
            assert_eq!(query["client_id"], oauth.client_id);
            assert_eq!(query["redirect_uri"], oauth.redirect_uri());
            assert_eq!(query["scope"], oauth.scope);
            assert_eq!(query["response_type"], "code");
            assert_eq!(query["code_challenge_method"], "S256");
            assert_eq!(query["code_challenge"], request.pkce.challenge());
            assert_eq!(query["state"], request.state);
            for (key, value) in oauth.extra_authorize_params {
                assert_eq!(query[*key], *value, "{key} missing for {provider}");
            }
        }
        assert!(
            authorize_request(Provider::Codex)
                .url
                .contains("localhost%3A1455%2Fauth%2Fcallback")
        );
    }

    #[tokio::test]
    async fn codex_expiry_comes_from_the_access_token_jwt() {
        let exp = 2_000_000_000;
        let (url, _) = fake_token_endpoint(
            StatusCode::OK,
            serde_json::json!({
                "access_token": codex_access_token(exp),
                "refresh_token": "refresh-2",
                // Deliberately a lie: Codex's expires_in is not to be trusted.
                "expires_in": 60,
            }),
            Duration::ZERO,
        )
        .await;

        let tokens = refresh_at(Provider::Codex, &url, "refresh-1")
            .await
            .unwrap();
        assert_eq!(
            tokens.expires_at,
            Some(Timestamp::from_second(exp).unwrap())
        );
        assert_eq!(tokens.refresh.as_deref(), Some("refresh-2"));
    }

    #[tokio::test]
    async fn claude_expiry_comes_from_expires_in() {
        let (url, _) = fake_token_endpoint(
            StatusCode::OK,
            serde_json::json!({ "access_token": "claude-access", "expires_in": 3600 }),
            Duration::ZERO,
        )
        .await;

        let before = Timestamp::now();
        let tokens = refresh_at(Provider::Claude, &url, "refresh-1")
            .await
            .unwrap();
        let expires_at = tokens.expires_at.unwrap();
        assert!(expires_at >= before + SignedDuration::from_secs(3600));
        assert!(expires_at <= Timestamp::now() + SignedDuration::from_secs(3601));
        assert_eq!(tokens.refresh.as_deref(), Some("refresh-1"));
    }

    #[tokio::test]
    async fn codex_posts_a_form_and_claude_posts_json() {
        let (url, seen) = fake_token_endpoint(
            StatusCode::OK,
            serde_json::json!({ "access_token": "a", "expires_in": 60 }),
            Duration::ZERO,
        )
        .await;

        refresh_at(Provider::Codex, &url, "refresh-1")
            .await
            .unwrap();
        assert!(
            seen.content_type
                .lock()
                .unwrap()
                .starts_with("application/x-www-form-urlencoded")
        );
        let body = seen.body.lock().unwrap().clone();
        assert!(body.contains("grant_type=refresh_token"), "{body}");
        // Codex drops offline_access on refresh.
        assert!(body.contains("scope=openid+profile+email"), "{body}");
        assert!(!body.contains("offline_access"), "{body}");

        refresh_at(Provider::Claude, &url, "refresh-1")
            .await
            .unwrap();
        assert!(
            seen.content_type
                .lock()
                .unwrap()
                .starts_with("application/json")
        );
        let body: Value = serde_json::from_str(&seen.body.lock().unwrap()).unwrap();
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["client_id"], Provider::Claude.oauth().client_id);
        // Claude reuses the login scope, so it sends none.
        assert!(body.get("scope").is_none(), "{body}");
    }

    #[tokio::test]
    async fn the_code_exchange_uses_the_same_body_encoding() {
        let (url, seen) = fake_token_endpoint(
            StatusCode::OK,
            serde_json::json!({ "access_token": "a", "expires_in": 60 }),
            Duration::ZERO,
        )
        .await;

        exchange_code_at(
            Provider::Claude,
            &url,
            "the-code",
            "the-verifier",
            "the-state",
        )
        .await
        .unwrap();
        let body: Value = serde_json::from_str(&seen.body.lock().unwrap()).unwrap();
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["code"], "the-code");
        assert_eq!(body["code_verifier"], "the-verifier");
        assert_eq!(body["state"], "the-state");

        exchange_code_at(
            Provider::Codex,
            &url,
            "the-code",
            "the-verifier",
            "the-state",
        )
        .await
        .unwrap();
        let body = seen.body.lock().unwrap().clone();
        assert!(body.contains("code_verifier=the-verifier"), "{body}");
        assert!(!body.contains("state="), "codex sends no state: {body}");
    }

    #[tokio::test]
    async fn only_400_401_and_403_are_permanent() {
        for (status, permanent) in [
            (StatusCode::BAD_REQUEST, true),
            (StatusCode::UNAUTHORIZED, true),
            (StatusCode::FORBIDDEN, true),
            (StatusCode::SERVICE_UNAVAILABLE, false),
            (StatusCode::INTERNAL_SERVER_ERROR, false),
            (StatusCode::BAD_GATEWAY, false),
            (StatusCode::TOO_MANY_REQUESTS, false),
            (StatusCode::NOT_FOUND, false),
        ] {
            let (url, _) = fake_token_endpoint(
                status,
                serde_json::json!({ "error": "invalid_grant" }),
                Duration::ZERO,
            )
            .await;
            let err = refresh_at(Provider::Codex, &url, "refresh-1")
                .await
                .unwrap_err();
            assert_eq!(err.permanent, permanent, "{status} -> {err}");
            assert!(err.message.contains(status.as_str()), "{err}");
        }
    }

    #[tokio::test]
    async fn a_network_failure_is_transient() {
        // Nothing is listening on this port.
        let err = refresh_at(Provider::Codex, "http://127.0.0.1:1/token", "refresh-1")
            .await
            .unwrap_err();
        assert!(!err.permanent, "{err}");
    }

    #[tokio::test]
    async fn a_malformed_success_body_is_transient() {
        let (url, _) = fake_token_endpoint(
            StatusCode::OK,
            serde_json::json!({ "no_access_token_here": true }),
            Duration::ZERO,
        )
        .await;
        let err = refresh_at(Provider::Claude, &url, "refresh-1")
            .await
            .unwrap_err();
        assert!(!err.permanent, "{err}");
    }

    #[test]
    fn the_error_code_is_read_from_either_provider_shape() {
        assert_eq!(
            error_code(r#"{"error":"invalid_grant"}"#).as_deref(),
            Some("invalid_grant")
        );
        assert_eq!(
            error_code(r#"{"error":{"type":"invalid_request_error"}}"#).as_deref(),
            Some("invalid_request_error")
        );
        assert_eq!(error_code("not json at all"), None);
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh() {
        let (url, seen) = fake_token_endpoint(
            StatusCode::OK,
            serde_json::json!({
                "access_token": codex_access_token(2_000_000_000),
                "refresh_token": "refresh-2",
            }),
            Duration::from_millis(150),
        )
        .await;

        let manager = Arc::new(TokenManager::with_token_urls(TokenUrls::all(url)));
        let expired = Some(Timestamp::now() - SignedDuration::from_secs(10));

        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let manager = manager.clone();
                let mut sub = sub(Provider::Codex, expired);
                tokio::spawn(async move {
                    manager.ensure_fresh(&mut sub, false).await.map(|changed| {
                        assert!(changed);
                        sub.credentials.tokens.access
                    })
                })
            })
            .collect();

        let mut tokens = Vec::new();
        for task in tasks {
            tokens.push(task.await.unwrap().unwrap());
        }
        assert_eq!(
            seen.hits.load(Ordering::SeqCst),
            1,
            "concurrent refreshes must hit the token endpoint exactly once"
        );
        assert!(tokens.windows(2).all(|w| w[0] == w[1]));
        assert!(manager.inflight.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_fresh_token_is_left_alone_unless_forced_or_inside_the_skew() {
        let (url, seen) = fake_token_endpoint(
            StatusCode::OK,
            serde_json::json!({ "access_token": "new-access", "expires_in": 3600 }),
            Duration::ZERO,
        )
        .await;
        let manager = TokenManager::with_token_urls(TokenUrls::all(url));

        let mut fresh = sub(
            Provider::Claude,
            Some(Timestamp::now() + SignedDuration::from_secs(3600)),
        );
        assert!(!manager.ensure_fresh(&mut fresh, false).await.unwrap());
        assert_eq!(seen.hits.load(Ordering::SeqCst), 0);
        assert_eq!(fresh.credentials.tokens.access, "stale-access");

        assert!(manager.ensure_fresh(&mut fresh, true).await.unwrap());
        assert_eq!(seen.hits.load(Ordering::SeqCst), 1);
        assert_eq!(fresh.credentials.tokens.access, "new-access");

        let mut nearly = sub(
            Provider::Claude,
            Some(Timestamp::now() + EXPIRY_SKEW - SignedDuration::from_secs(1)),
        );
        assert!(manager.ensure_fresh(&mut nearly, false).await.unwrap());
        assert_eq!(seen.hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_unchanged_access_token_reports_no_change() {
        let (url, _) = fake_token_endpoint(
            StatusCode::OK,
            serde_json::json!({ "access_token": "stale-access", "expires_in": 3600 }),
            Duration::ZERO,
        )
        .await;
        let manager = TokenManager::with_token_urls(TokenUrls::all(url));
        let mut sub = sub(Provider::Claude, None);
        assert!(!manager.ensure_fresh(&mut sub, true).await.unwrap());
    }

    #[tokio::test]
    async fn a_sub_without_a_refresh_token_fails_permanently() {
        let manager = TokenManager::with_token_urls(TokenUrls::all("http://127.0.0.1:1/token"));
        let mut sub = sub(Provider::Claude, None);
        sub.credentials.tokens.refresh = None;
        let err = manager.ensure_fresh(&mut sub, false).await.unwrap_err();
        assert!(err.permanent, "{err}");
    }

    #[tokio::test]
    async fn a_transient_failure_leaves_the_tokens_untouched() {
        let (url, _) = fake_token_endpoint(
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({ "error": "overloaded" }),
            Duration::ZERO,
        )
        .await;
        let manager = TokenManager::with_token_urls(TokenUrls::all(url));
        let mut sub = sub(Provider::Codex, None);
        let err = manager.ensure_fresh(&mut sub, false).await.unwrap_err();
        assert!(!err.permanent, "{err}");
        assert_eq!(sub.credentials.tokens.access, "stale-access");
        assert!(manager.inflight.lock().unwrap().is_empty());
    }

    fn codex_auth_file(access_exp: i64, refresh: &str) -> String {
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": jwt(&serde_json::json!({
                    "https://api.openai.com/auth": { "chatgpt_account_id": "acct-1" },
                })),
                "access_token": codex_access_token(access_exp),
                "refresh_token": refresh,
                "account_id": "acct-1",
            },
        })
        .to_string()
    }

    /// A token endpoint that honours one refresh token and answers
    /// `400 invalid_grant` for every other — a rotated-away grant.
    async fn rotating_token_endpoint(good: &'static str) -> (String, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        let app = Router::new()
            .route(
                "/token",
                post(
                    move |State(seen): State<Arc<Seen>>, raw: String| async move {
                        seen.hits.fetch_add(1, Ordering::SeqCst);
                        *seen.body.lock().unwrap() = raw.clone();
                        if raw.contains(good) {
                            (
                                StatusCode::OK,
                                axum::Json(serde_json::json!({
                                    "access_token": codex_access_token(4_000_000_000),
                                    "refresh_token": "rt-issued-by-us",
                                    "expires_in": 3600,
                                })),
                            )
                        } else {
                            (
                                StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({ "error": "invalid_grant" })),
                            )
                        }
                    },
                ),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (url, seen)
    }

    /// A sub adopted from `path`, holding a refresh token the vendor has since
    /// rotated away, and an access token long expired.
    fn adopted_sub(path: &std::path::Path) -> Sub {
        let account =
            discovery::parse_codex_auth(&codex_auth_file(1, "rt-dead"), path).expect("an account");
        account.into_sub()
    }

    #[tokio::test]
    async fn an_adopted_sub_recovers_from_a_refresh_token_the_vendor_rotated() {
        let (url, seen) = rotating_token_endpoint("rt-live").await;
        let dir = crate::store::tests_support::temp_dir("auth-recover-rotated");
        let path = dir.join("auth.json");
        // `codex` holds a newer refresh token, and an expired access token, so
        // recovery has to spend the new grant.
        let source = codex_auth_file(1, "rt-live");
        std::fs::write(&path, &source).expect("seed auth.json");

        let manager = TokenManager::with_token_urls(TokenUrls::all(url));
        let mut sub = adopted_sub(&path);
        let changed = manager
            .ensure_fresh(&mut sub, false)
            .await
            .expect("the re-read credential recovers it");

        assert!(changed);
        assert_eq!(
            sub.credentials.tokens.refresh.as_deref(),
            Some("rt-issued-by-us")
        );
        assert_eq!(
            seen.hits.load(Ordering::SeqCst),
            2,
            "one doomed refresh, then exactly one retry with the re-read token"
        );
        // The read-only promise: we never write the vendor's file.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    }

    #[tokio::test]
    async fn a_still_valid_credential_in_the_source_is_adopted_without_a_refresh() {
        let (url, seen) = rotating_token_endpoint("nothing-matches-this").await;
        let dir = crate::store::tests_support::temp_dir("auth-recover-valid");
        let path = dir.join("auth.json");
        // `codex` refreshed a moment ago: its access token is good for hours.
        std::fs::write(&path, codex_auth_file(4_000_000_000, "rt-live")).expect("seed auth.json");

        let manager = TokenManager::with_token_urls(TokenUrls::all(url));
        let mut sub = adopted_sub(&path);
        assert!(manager.ensure_fresh(&mut sub, false).await.unwrap());

        assert_eq!(sub.credentials.tokens.refresh.as_deref(), Some("rt-live"));
        assert_eq!(
            seen.hits.load(Ordering::SeqCst),
            1,
            "spending the vendor's grant would rotate it out from under `codex`"
        );
    }

    #[tokio::test]
    async fn the_same_dead_credential_in_the_source_still_means_needs_login() {
        let (url, seen) = rotating_token_endpoint("rt-live").await;
        let dir = crate::store::tests_support::temp_dir("auth-recover-dead");
        let path = dir.join("auth.json");
        // The source holds exactly what we already tried.
        std::fs::write(&path, codex_auth_file(1, "rt-dead")).expect("seed auth.json");

        let manager = TokenManager::with_token_urls(TokenUrls::all(url));
        let mut sub = adopted_sub(&path);
        let err = manager.ensure_fresh(&mut sub, false).await.unwrap_err();

        assert!(err.permanent, "{err}");
        assert!(err.message.contains("invalid_grant"), "{err}");
        assert_eq!(
            seen.hits.load(Ordering::SeqCst),
            1,
            "no point retrying a grant we already know is dead"
        );
    }

    #[tokio::test]
    async fn a_sub_from_subbiers_own_login_goes_straight_to_needs_login() {
        let (url, seen) = rotating_token_endpoint("rt-live").await;
        let manager = TokenManager::with_token_urls(TokenUrls::all(url));
        let mut sub = sub(Provider::Codex, None);
        assert!(matches!(sub.credentials.source, CredentialSource::Subbier));

        let err = manager.ensure_fresh(&mut sub, false).await.unwrap_err();
        assert!(err.permanent, "{err}");
        assert_eq!(
            seen.hits.load(Ordering::SeqCst),
            1,
            "there is no source to re-read"
        );
    }

    #[tokio::test]
    async fn the_source_is_reread_once_per_failure_not_once_per_caller() {
        let (url, seen) = rotating_token_endpoint("rt-live").await;
        let dir = crate::store::tests_support::temp_dir("auth-recover-deduped");
        let path = dir.join("auth.json");
        std::fs::write(&path, codex_auth_file(1, "rt-live")).expect("seed auth.json");

        let manager = Arc::new(TokenManager::with_token_urls(TokenUrls::all(url)));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let manager = manager.clone();
                let mut sub = adopted_sub(&path);
                tokio::spawn(async move { manager.ensure_fresh(&mut sub, false).await.map(|_| ()) })
            })
            .collect();
        for task in tasks {
            task.await.unwrap().expect("every caller recovers");
        }

        assert_eq!(
            seen.hits.load(Ordering::SeqCst),
            2,
            "the recovery rides on the dedupe cell: one failure, one retry"
        );
    }

    #[test]
    fn jwt_claims_decode_without_verifying_a_signature() {
        let token = codex_access_token(1_234_567_890);
        let claims = jwt_claims(&token).unwrap();
        assert_eq!(claims["email"], "me@example.com");
        assert_eq!(
            access_token_expiry(&token),
            Some(Timestamp::from_second(1_234_567_890).unwrap())
        );

        assert!(jwt_claims("not-a-jwt").is_none());
        assert!(jwt_claims("a.!!!not-base64!!!.c").is_none());
        assert!(access_token_expiry("a.eyJzdWIiOiJubyBleHAifQ.c").is_none());
    }

    #[test]
    fn identity_is_read_from_whichever_shape_the_provider_used() {
        let codex = serde_json::json!({ "id_token": codex_access_token(1) });
        assert_eq!(account_id_from(&codex, ""), Some("acct-1".into()));
        assert_eq!(email_from(&codex, ""), Some("me@example.com".into()));

        let claude = serde_json::json!({
            "account": { "email_address": "me@example.com", "uuid": "acct-2" },
            "organization": { "uuid": "org-9" },
        });
        assert_eq!(account_id_from(&claude, ""), Some("org-9".into()));
        assert_eq!(email_from(&claude, ""), Some("me@example.com".into()));

        let empty = serde_json::json!({});
        assert_eq!(account_id_from(&empty, ""), None);
        assert_eq!(email_from(&empty, ""), None);
    }

    #[test]
    fn keys_and_labels_fall_back_predictably() {
        let mut credentials = Credentials {
            plan: None,
            account_id: Some("acct-1".into()),
            email: Some("me@example.com".into()),
            tokens: Tokens {
                access: "a".into(),
                refresh: None,
                expires_at: None,
            },
            source: CredentialSource::Subbier,
        };
        assert_eq!(
            account_key(Provider::Codex, &credentials).as_str(),
            "codex:acct-1"
        );
        assert_eq!(
            default_label(Provider::Codex, &credentials),
            "me@example.com"
        );

        credentials.account_id = None;
        assert_eq!(
            account_key(Provider::Codex, &credentials).as_str(),
            "codex:me@example.com"
        );

        credentials.email = None;
        assert_eq!(
            account_key(Provider::Claude, &credentials).as_str(),
            "claude:default"
        );
        assert_eq!(default_label(Provider::Claude, &credentials), "Claude");
    }
}
