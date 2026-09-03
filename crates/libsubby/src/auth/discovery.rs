//! Zero-config discovery: adopt the accounts `codex` and `claude` already hold;
//! only *additional* accounts need [`crate::auth::login`]. Best-effort and
//! read-only — a missing file, a denied Keychain prompt or malformed JSON yields
//! no account and a `debug` log, never an error, a write or a logged secret.

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde_json::Value;

use crate::auth::{access_token_expiry, account_key, default_label, jwt_claims};
use crate::model::{CredentialSource, Credentials, Provider, Sub, SubKey, Tokens};

/// The macOS Keychain generic-password service Claude Code stores its OAuth
/// credentials under; exact, including the space.
pub const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredAccount {
    pub key: SubKey,
    pub provider: Provider,
    /// Email, else organisation name, else the provider's display name.
    pub label: String,
    pub credentials: Credentials,
}

impl DiscoveredAccount {
    #[must_use]
    pub fn into_sub(self) -> Sub {
        Sub {
            key: self.key,
            provider: self.provider,
            label: self.label,
            credentials: self.credentials,
        }
    }
}

/// Who `~/.claude.json` says is logged in — Claude Code keeps the identity and
/// the credentials in two different places.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeIdentity {
    pub email: Option<String>,
    pub organization_name: Option<String>,
    /// Organisation uuid when present, else the account uuid.
    pub account_id: Option<String>,
}

/// Every account subbier can adopt from the local machine.
#[must_use]
pub fn discover() -> Vec<DiscoveredAccount> {
    let found: Vec<DiscoveredAccount> = Provider::ALL
        .into_iter()
        .flat_map(discover_provider)
        .collect();
    tracing::debug!(accounts = found.len(), "account discovery finished");
    found
}

fn discover_provider(p: Provider) -> Vec<DiscoveredAccount> {
    let found: Vec<DiscoveredAccount> = match p {
        Provider::Codex => codex_account_at(&codex_auth_path()).into_iter().collect(),
        Provider::Claude => claude_accounts(),
    };
    tracing::debug!(provider = %p, accounts = found.len(), "discovered accounts for provider");
    found
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// `$CODEX_HOME/auth.json`, else `~/.codex/auth.json`.
#[must_use]
pub fn codex_auth_path() -> PathBuf {
    match std::env::var_os("CODEX_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home().join(".codex"),
    }
    .join("auth.json")
}

/// `$CLAUDE_CONFIG_DIR/.credentials.json`, else `~/.claude/.credentials.json`.
/// On macOS the Keychain wins and this is the fallback.
#[must_use]
pub fn claude_credentials_path() -> PathBuf {
    claude_config_dir().join(".credentials.json")
}

/// `$CLAUDE_CONFIG_DIR/.claude.json`, else `~/.claude.json` — the identity file.
#[must_use]
pub fn claude_identity_path() -> PathBuf {
    match claude_config_dir_override() {
        Some(dir) => dir.join(".claude.json"),
        None => home().join(".claude.json"),
    }
}

fn claude_config_dir() -> PathBuf {
    claude_config_dir_override().unwrap_or_else(|| home().join(".claude"))
}

fn claude_config_dir_override() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

fn read_best_effort(path: &Path, what: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "no {what} to adopt");
            None
        }
    }
}

#[must_use]
pub fn codex_account_at(path: &Path) -> Option<DiscoveredAccount> {
    let text = read_best_effort(path, "codex auth.json")?;
    parse_codex_auth(&text, path)
}

/// Parse a Codex `auth.json` payload. `None` for malformed JSON and for an
/// API-key-only `auth.json`, which has no OAuth tokens to adopt.
#[must_use]
pub fn parse_codex_auth(json: &str, from: &Path) -> Option<DiscoveredAccount> {
    let value: Value = match serde_json::from_str(json) {
        Ok(value) => value,
        Err(e) => {
            tracing::debug!(path = %from.display(), error = %e, "codex auth.json is not valid JSON");
            return None;
        }
    };
    let tokens = value.get("tokens")?;
    let access = tokens.get("access_token")?.as_str()?.to_owned();

    let id_claims = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(jwt_claims);
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            id_claims
                .as_ref()?
                .get("https://api.openai.com/auth")?
                .get("chatgpt_account_id")?
                .as_str()
                .map(str::to_owned)
        });
    let email = id_claims
        .as_ref()
        .and_then(|c| c.get("email")?.as_str().map(str::to_owned));

    let credentials = Credentials {
        account_id,
        email,
        // Codex states its plan on the usage endpoint, not in auth.json.
        plan: None,
        tokens: Tokens {
            // `last_refresh` is not an expiry; the access token's `exp` is.
            expires_at: access_token_expiry(&access),
            access,
            refresh: tokens
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        source: CredentialSource::Adopted {
            from: from.to_path_buf(),
        },
    };

    Some(account(Provider::Codex, credentials, None))
}

fn claude_accounts() -> Vec<DiscoveredAccount> {
    let identity = identity();

    // macOS keeps the payload in the Keychain; everywhere else it is a file.
    if let Some(payload) = read_keychain_credentials()
        && let Some(account) =
            parse_claude_credentials(&payload, identity.as_ref(), CredentialSource::Keychain)
    {
        return vec![account];
    }

    claude_account_at(&claude_credentials_path(), identity.as_ref())
        .into_iter()
        .collect()
}

#[must_use]
pub fn claude_account_at(
    path: &Path,
    identity: Option<&ClaudeIdentity>,
) -> Option<DiscoveredAccount> {
    let text = read_best_effort(path, "claude credentials")?;
    parse_claude_credentials(
        &text,
        identity,
        CredentialSource::Adopted {
            from: path.to_path_buf(),
        },
    )
}

/// Read the Claude Code credential blob out of the macOS Keychain. A denied
/// prompt, a missing item or a missing `security` binary are all just `None`.
#[must_use]
pub fn read_keychain_credentials() -> Option<String> {
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("security")
            .args(["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE, "-w"])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                let payload = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                (!payload.is_empty()).then_some(payload)
            }
            Ok(output) => {
                // Status only: stderr risks quoting the payload into a log.
                tracing::debug!(
                    status = %output.status,
                    service = CLAUDE_KEYCHAIN_SERVICE,
                    "no claude credentials in the keychain (or access was denied)"
                );
                None
            }
            Err(e) => {
                tracing::debug!(error = %e, "could not run `security` to read the keychain");
                None
            }
        }
    }
}

/// Parse a `{"claudeAiOauth": {…}}` payload, from the Keychain or from
/// `.credentials.json` — they are the same shape.
#[must_use]
pub fn parse_claude_credentials(
    json: &str,
    identity: Option<&ClaudeIdentity>,
    source: CredentialSource,
) -> Option<DiscoveredAccount> {
    let value: Value = match serde_json::from_str(json) {
        Ok(value) => value,
        Err(e) => {
            tracing::debug!(error = %e, "claude credentials are not valid JSON");
            return None;
        }
    };
    let oauth = value.get("claudeAiOauth")?;
    let access = oauth.get("accessToken")?.as_str()?.to_owned();

    let credentials = Credentials {
        account_id: identity.and_then(|i| i.account_id.clone()),
        email: identity.and_then(|i| i.email.clone()),
        // The only statement of a Claude plan on the machine; Max 5x and 20x
        // both spell themselves "max", which `plan::PlanTier` resolves down.
        plan: oauth
            .get("subscriptionType")
            .and_then(Value::as_str)
            .map(str::to_owned),
        tokens: Tokens {
            access,
            refresh: oauth
                .get("refreshToken")
                .and_then(Value::as_str)
                .map(str::to_owned),
            // `expiresAt` is epoch **milliseconds**.
            expires_at: oauth
                .get("expiresAt")
                .and_then(Value::as_i64)
                .and_then(|ms| Timestamp::from_millisecond(ms).ok()),
        },
        source,
    };

    Some(account(Provider::Claude, credentials, identity))
}

#[must_use]
pub fn claude_identity_at(path: &Path) -> Option<ClaudeIdentity> {
    let text = read_best_effort(path, "claude identity")?;
    parse_claude_identity(&text)
}

/// Parse `oauthAccount` out of a `~/.claude.json` payload.
#[must_use]
pub fn parse_claude_identity(json: &str) -> Option<ClaudeIdentity> {
    let value: Value = match serde_json::from_str(json) {
        Ok(value) => value,
        Err(e) => {
            tracing::debug!(error = %e, "~/.claude.json is not valid JSON");
            return None;
        }
    };
    let account = value.get("oauthAccount")?;
    let string = |key: &str| {
        account
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Some(ClaudeIdentity {
        email: string("emailAddress"),
        organization_name: string("organizationName"),
        // The organisation is the identity a Claude subscription bills against.
        account_id: string("organizationUuid").or_else(|| string("accountUuid")),
    })
}

/// Re-read `sub`'s credentials from the source they were adopted from. `None`
/// for a sub from subbier's own OAuth flow, a source that has disappeared, or
/// a source that now holds a different account.
///
/// **Blocking.** It reads a file and, for the Keychain, runs `security`.
#[must_use]
pub fn reread(sub: &Sub) -> Option<Credentials> {
    let found = match &sub.credentials.source {
        // `subs.json` already holds the freshest copy in existence.
        CredentialSource::Subbier => None,
        CredentialSource::Keychain => read_keychain_credentials().and_then(|payload| {
            parse_claude_credentials(&payload, identity().as_ref(), CredentialSource::Keychain)
        }),
        CredentialSource::Adopted { from } => reread_file(from),
    }?;

    if found.key != sub.key {
        tracing::debug!(
            sub = %sub.key,
            now = %found.key,
            "the adopted source now holds a different account; not adopting it here"
        );
        return None;
    }
    Some(found.credentials)
}

/// Parse whichever adopted credential file `path` is, by its shape: the two
/// payloads are disjoint, so this needs no `match` on [`Provider`].
fn reread_file(path: &Path) -> Option<DiscoveredAccount> {
    let text = read_best_effort(path, "adopted credentials")?;
    parse_codex_auth(&text, path).or_else(|| {
        parse_claude_credentials(
            &text,
            identity().as_ref(),
            CredentialSource::Adopted {
                from: path.to_path_buf(),
            },
        )
    })
}

fn identity() -> Option<ClaudeIdentity> {
    claude_identity_at(&claude_identity_path())
}

fn account(
    p: Provider,
    credentials: Credentials,
    identity: Option<&ClaudeIdentity>,
) -> DiscoveredAccount {
    let label = credentials
        .email
        .clone()
        .or_else(|| identity.and_then(|i| i.organization_name.clone()));
    DiscoveredAccount {
        key: account_key(p, &credentials),
        provider: p,
        label: label.unwrap_or_else(|| default_label(p, &credentials)),
        credentials,
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::*;

    fn jwt(claims: &Value) -> String {
        format!(
            "{}.{}.",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    fn codex_auth_json() -> String {
        let id_token = jwt(&serde_json::json!({
            "email": "me@example.com",
            "https://api.openai.com/auth": { "chatgpt_account_id": "4575f150-abc" },
        }));
        let access_token = jwt(&serde_json::json!({ "exp": 1_787_798_509_i64 }));
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": id_token,
                "access_token": access_token,
                "refresh_token": "rt-supersecret",
                "account_id": "4575f150-abc",
            },
            "last_refresh": "2026-08-26T09:00:00.000Z",
        })
        .to_string()
    }

    const CLAUDE_KEYCHAIN_JSON: &str = r#"{
      "claudeAiOauth": {
        "accessToken": "at-supersecret",
        "refreshToken": "rt-supersecret",
        "expiresAt": 1787798509000,
        "refreshTokenExpiresAt": 1790390509000,
        "scopes": ["user:inference", "user:profile"],
        "subscriptionType": "max",
        "rateLimitTier": "default"
      }
    }"#;

    const CLAUDE_IDENTITY_JSON: &str = r#"{
      "numStartups": 42,
      "oauthAccount": {
        "accountUuid": "acct-uuid-1",
        "emailAddress": "me@example.com",
        "organizationUuid": "org-uuid-9",
        "organizationName": "Example Inc",
        "organizationRole": "admin",
        "billingType": "subscription"
      }
    }"#;

    fn adopted_codex_sub(path: &Path) -> Sub {
        parse_codex_auth(&codex_auth_json(), path)
            .expect("a codex account")
            .into_sub()
    }

    #[test]
    #[ignore = "reads the real ~/.codex/auth.json and macOS Keychain"]
    fn rereading_a_real_adopted_sub_finds_the_live_credential() {
        let accounts = discover();
        assert!(
            !accounts.is_empty(),
            "nothing is logged in on this machine; there is nothing to re-read"
        );
        for account in accounts {
            let sub = account.into_sub();
            let live = reread(&sub).expect("what discovery just read is still there");
            assert_eq!(live.tokens, sub.credentials.tokens);

            let mut stale = sub.clone();
            stale.credentials.tokens.access = "at-dead".into();
            stale.credentials.tokens.refresh = Some("rt-rotated-away".into());
            let recovered = reread(&stale).expect("the live credential is still there");
            assert_ne!(recovered.tokens.access, stale.credentials.tokens.access);
            assert_ne!(recovered.tokens.refresh, stale.credentials.tokens.refresh);
        }
    }

    #[test]
    fn rereading_an_adopted_file_picks_up_a_rotated_refresh_token() {
        let dir = crate::store::tests_support::temp_dir("discovery-reread");
        let path = dir.join("auth.json");
        std::fs::write(&path, codex_auth_json()).expect("seed auth.json");
        let sub = adopted_codex_sub(&path);
        assert_eq!(
            sub.credentials.tokens.refresh.as_deref(),
            Some("rt-supersecret")
        );

        // `codex` rotates the refresh token out from under us.
        let rotated = codex_auth_json().replace("rt-supersecret", "rt-rotated");
        std::fs::write(&path, &rotated).expect("rotate auth.json");

        let found = reread(&sub).expect("the source still holds this account");
        assert_eq!(found.tokens.refresh.as_deref(), Some("rt-rotated"));
        // Re-reading is a read: the source is byte-for-byte what codex left.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), rotated);
    }

    #[test]
    fn nothing_is_reread_unless_the_source_still_holds_this_account() {
        let dir = crate::store::tests_support::temp_dir("discovery-reread-none");
        let path = dir.join("auth.json");
        std::fs::write(&path, codex_auth_json()).expect("seed auth.json");
        let sub = adopted_codex_sub(&path);

        let mut own = sub.clone();
        own.credentials.source = CredentialSource::Subbier;
        assert_eq!(reread(&own), None, "subbier's own flow has no source");

        // The user signed `codex` into a different ChatGPT account.
        let other = codex_auth_json().replace("4575f150-abc", "9999aaaa-zzz");
        std::fs::write(&path, other).expect("swap accounts");
        assert_eq!(reread(&sub), None);

        std::fs::remove_file(&path).expect("remove auth.json");
        assert_eq!(reread(&sub), None);
    }

    #[test]
    fn an_adopted_file_is_recognised_by_its_shape_not_by_its_provider() {
        let dir = crate::store::tests_support::temp_dir("discovery-reread-shape");
        let codex = dir.join("auth.json");
        std::fs::write(&codex, codex_auth_json()).expect("seed codex");
        let claude = dir.join(".credentials.json");
        std::fs::write(&claude, CLAUDE_KEYCHAIN_JSON).expect("seed claude");

        assert_eq!(reread_file(&codex).unwrap().provider, Provider::Codex);
        let found = reread_file(&claude).expect("a claude account");
        assert_eq!(found.provider, Provider::Claude);
        assert_eq!(
            found.credentials.tokens.refresh.as_deref(),
            Some("rt-supersecret")
        );
        assert_eq!(reread_file(&dir.join("nothing-here.json")), None);
    }

    #[test]
    fn a_codex_auth_json_becomes_an_adopted_account() {
        let path = Path::new("/home/me/.codex/auth.json");
        let account = parse_codex_auth(&codex_auth_json(), path).unwrap();

        assert_eq!(account.provider, Provider::Codex);
        assert_eq!(account.key.as_str(), "codex:4575f150-abc");
        assert_eq!(account.label, "me@example.com");
        assert_eq!(
            account.credentials.account_id.as_deref(),
            Some("4575f150-abc")
        );
        assert_eq!(account.credentials.email.as_deref(), Some("me@example.com"));
        assert_eq!(
            account.credentials.tokens.refresh.as_deref(),
            Some("rt-supersecret")
        );
        // Expiry is the access token's own JWT `exp`, not `last_refresh`.
        assert_eq!(
            account.credentials.tokens.expires_at,
            Some(Timestamp::from_second(1_787_798_509).unwrap())
        );
        assert_eq!(
            account.credentials.source,
            CredentialSource::Adopted {
                from: path.to_path_buf()
            }
        );
    }

    #[test]
    fn a_codex_account_id_falls_back_to_the_id_token_claim() {
        let id_token = jwt(&serde_json::json!({
            "email": "me@example.com",
            "https://api.openai.com/auth": { "chatgpt_account_id": "from-claim" },
        }));
        let json = serde_json::json!({
            "tokens": { "id_token": id_token, "access_token": "opaque" }
        })
        .to_string();

        let account = parse_codex_auth(&json, Path::new("auth.json")).unwrap();
        assert_eq!(
            account.credentials.account_id.as_deref(),
            Some("from-claim")
        );
        // No known expiry counts as stale, so we refresh before sending.
        assert_eq!(account.credentials.tokens.expires_at, None);
        assert_eq!(account.credentials.tokens.refresh, None);
    }

    #[test]
    fn an_api_key_only_codex_auth_json_yields_nothing() {
        let json = r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-supersecret","tokens":null}"#;
        assert!(parse_codex_auth(json, Path::new("auth.json")).is_none());
    }

    #[test]
    fn a_keychain_payload_plus_an_identity_becomes_an_account() {
        let identity = parse_claude_identity(CLAUDE_IDENTITY_JSON).unwrap();
        assert_eq!(identity.email.as_deref(), Some("me@example.com"));
        assert_eq!(identity.organization_name.as_deref(), Some("Example Inc"));
        assert_eq!(identity.account_id.as_deref(), Some("org-uuid-9"));

        let account = parse_claude_credentials(
            CLAUDE_KEYCHAIN_JSON,
            Some(&identity),
            CredentialSource::Keychain,
        )
        .unwrap();

        assert_eq!(account.provider, Provider::Claude);
        assert_eq!(account.key.as_str(), "claude:org-uuid-9");
        assert_eq!(account.label, "me@example.com");
        assert_eq!(account.credentials.source, CredentialSource::Keychain);
        assert_eq!(
            account.credentials.tokens.refresh.as_deref(),
            Some("rt-supersecret")
        );
        // expiresAt is epoch MILLISECONDS.
        assert_eq!(
            account.credentials.tokens.expires_at,
            Some(Timestamp::from_second(1_787_798_509).unwrap())
        );
    }

    #[test]
    fn an_account_with_no_email_falls_back_to_the_organisation_then_the_provider() {
        let identity = ClaudeIdentity {
            email: None,
            organization_name: Some("Example Inc".into()),
            account_id: Some("org-uuid-9".into()),
        };
        let account = parse_claude_credentials(
            CLAUDE_KEYCHAIN_JSON,
            Some(&identity),
            CredentialSource::Keychain,
        )
        .unwrap();
        assert_eq!(account.key.as_str(), "claude:org-uuid-9");
        assert_eq!(account.label, "Example Inc");

        let account =
            parse_claude_credentials(CLAUDE_KEYCHAIN_JSON, None, CredentialSource::Keychain)
                .unwrap();
        assert_eq!(account.key.as_str(), "claude:default");
        assert_eq!(account.label, "Claude");
        assert_eq!(account.credentials.email, None);
    }

    #[test]
    fn malformed_and_missing_inputs_yield_nothing_and_never_panic() {
        for json in ["", "{", "null", "[]", "{}", r#"{"tokens":{}}"#] {
            assert!(
                parse_codex_auth(json, Path::new("auth.json")).is_none(),
                "{json}"
            );
            assert!(
                parse_claude_credentials(json, None, CredentialSource::Keychain).is_none(),
                "{json}"
            );
            assert!(parse_claude_identity(json).is_none(), "{json}");
        }

        let missing = Path::new("/nonexistent/subbier-test/auth.json");
        assert!(codex_account_at(missing).is_none());
        assert!(claude_identity_at(missing).is_none());
        assert!(claude_account_at(missing, None).is_none());
    }

    #[test]
    fn a_credentials_file_is_adopted_the_same_way_as_the_keychain() {
        let dir = crate::store::tests_support::temp_dir("discovery-credentials-file");
        let path = dir.join(".credentials.json");
        std::fs::write(&path, CLAUDE_KEYCHAIN_JSON).unwrap();

        let identity = parse_claude_identity(CLAUDE_IDENTITY_JSON).unwrap();
        let account = claude_account_at(&path, Some(&identity)).unwrap();
        assert_eq!(account.key.as_str(), "claude:org-uuid-9");
        assert_eq!(
            account.credentials.source,
            CredentialSource::Adopted { from: path.clone() }
        );
    }

    #[test]
    fn debug_output_never_reveals_a_token() {
        let account =
            parse_claude_credentials(CLAUDE_KEYCHAIN_JSON, None, CredentialSource::Keychain)
                .unwrap();
        let rendered = format!("{account:?}");
        assert!(!rendered.contains("supersecret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }
}
