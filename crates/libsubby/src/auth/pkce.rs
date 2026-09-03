//! PKCE (RFC 7636) and the one-shot loopback callback server.
//!
//! Verifiers and unexchanged authorization codes are secrets: [`Pkce`] and
//! [`Callback`] hand-write redacting [`fmt::Debug`] impls — do not derive it.

use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng as _;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};

use crate::error::{Error, Result};

/// 43 base64url characters, the RFC 7636 minimum.
pub const VERIFIER_BYTES: usize = 32;

#[derive(Clone, PartialEq, Eq)]
pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    #[must_use]
    pub fn generate() -> Self {
        Self::from_verifier(random_urlsafe(VERIFIER_BYTES))
    }

    /// The pair derived from an existing verifier; prefer [`Pkce::generate`].
    #[must_use]
    pub fn from_verifier(verifier: impl Into<String>) -> Self {
        let verifier = verifier.into();
        let challenge = challenge_for(&verifier);
        Self {
            verifier,
            challenge,
        }
    }

    /// **Secret.**
    #[must_use]
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    #[must_use]
    pub fn challenge(&self) -> &str {
        &self.challenge
    }
}

impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier", &"<redacted>")
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// `base64url(sha256(verifier))`, unpadded — the S256 transformation.
#[must_use]
pub fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// `base64url(random bytes)`, unpadded.
#[must_use]
pub fn random_urlsafe(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill(&mut buffer[..]);
    URL_SAFE_NO_PAD.encode(&buffer)
}

#[derive(Clone, PartialEq, Eq)]
pub struct Callback {
    /// **Secret until exchanged.**
    pub code: String,
    /// The `state` the provider echoed, or `""` if it sent none.
    pub state: String,
}

impl fmt::Debug for Callback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Callback")
            .field("code", &"<redacted>")
            .field("state", &"<redacted>")
            .finish()
    }
}

const SUCCESS_HTML: &str = "<!doctype html><meta charset=\"utf-8\"><title>Signed in</title>\
<body style=\"font-family:system-ui,sans-serif;text-align:center;padding-top:4rem\">\
<h2>Signed in</h2><p>You can close this tab and return to subbier.</p>";

// The provider's error text is never reflected here: it would be
// attacker-influenced markup on a localhost origin.
const FAILURE_HTML: &str = "<!doctype html><meta charset=\"utf-8\"><title>Sign-in failed</title>\
<body style=\"font-family:system-ui,sans-serif;text-align:center;padding-top:4rem\">\
<h2>Sign-in failed</h2><p>Return to subbier for the details.</p>";

struct Inner {
    path: String,
    /// `None` once the one request we serve has been answered.
    sender: Mutex<Option<oneshot::Sender<Result<Callback>>>>,
    done: Arc<Notify>,
}

impl Inner {
    /// Deliver the outcome and stop the server; a later request gets a 410.
    fn finish(&self, outcome: Result<Callback>) -> bool {
        let sender = self.sender.lock().expect("callback sender mutex").take();
        match sender {
            Some(sender) => {
                let _ = sender.send(outcome);
                self.done.notify_one();
                true
            }
            None => false,
        }
    }
}

/// A one-shot OAuth callback listener, bound to 127.0.0.1 only so the callback
/// is not offered to the whole LAN. Dropping it releases the port immediately.
pub struct CodeWaiter {
    local_addr: SocketAddr,
    receiver: oneshot::Receiver<Result<Callback>>,
    cancel: Arc<Notify>,
}

impl CodeWaiter {
    /// Bind `port` and start serving.
    ///
    /// A port that will not bind usually means the vendor's own CLI is on it.
    pub async fn bind(port: u16, path: &str) -> Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|e| {
                Error::auth(format!(
                    "cannot listen on 127.0.0.1:{port} for the OAuth callback ({e}); \
                     the provider's own CLI or another subbier login is probably \
                     already using that port — close it and try again"
                ))
            })?;
        let local_addr = listener.local_addr()?;

        let (sender, receiver) = oneshot::channel();
        let done = Arc::new(Notify::new());
        let cancel = Arc::new(Notify::new());
        let inner = Arc::new(Inner {
            path: path.to_owned(),
            sender: Mutex::new(Some(sender)),
            done: done.clone(),
        });

        let app = Router::new().fallback(handle).with_state(inner);
        let shutdown = {
            let cancel = cancel.clone();
            async move {
                tokio::select! {
                    () = done.notified() => {}
                    () = cancel.notified() => {}
                }
            }
        };
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
            {
                tracing::debug!(error = %e, "oauth callback server stopped");
            }
        });

        Ok(Self {
            local_addr,
            receiver,
            cancel,
        })
    }

    /// The address actually bound — only interesting when `port` was 0.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Wait for the one callback.
    pub async fn recv(&mut self) -> Result<Callback> {
        match (&mut self.receiver).await {
            Ok(outcome) => outcome,
            Err(_) => Err(Error::auth("the sign-in callback was cancelled")),
        }
    }
}

impl fmt::Debug for CodeWaiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodeWaiter")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl Drop for CodeWaiter {
    fn drop(&mut self) {
        self.cancel.notify_one();
    }
}

async fn handle(State(inner): State<Arc<Inner>>, uri: Uri) -> Response {
    if uri.path() != inner.path {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let mut code = None;
    let mut state = String::new();
    let mut error = None;
    let mut description = None;
    for (key, value) in url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
        match &*key {
            "code" => code = Some(value.into_owned()),
            "state" => state = value.into_owned(),
            "error" => error = Some(value.into_owned()),
            "error_description" => description = Some(value.into_owned()),
            _ => {}
        }
    }

    let (outcome, response) = match code {
        Some(code) => (
            Ok(Callback { code, state }),
            (StatusCode::OK, Html(SUCCESS_HTML)).into_response(),
        ),
        None => {
            let detail = description
                .or(error)
                .unwrap_or_else(|| "no authorization code in the callback".to_owned());
            (
                Err(Error::auth(format!("sign-in failed: {detail}"))),
                (StatusCode::BAD_REQUEST, Html(FAILURE_HTML)).into_response(),
            )
        }
    };

    if inner.finish(outcome) {
        response
    } else {
        (StatusCode::GONE, "this sign-in has already completed").into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 appendix B.
    const RFC_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const RFC_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    #[test]
    fn s256_matches_the_rfc_7636_test_vector() {
        assert_eq!(challenge_for(RFC_VERIFIER), RFC_CHALLENGE);
        let pkce = Pkce::from_verifier(RFC_VERIFIER);
        assert_eq!(pkce.verifier(), RFC_VERIFIER);
        assert_eq!(pkce.challenge(), RFC_CHALLENGE);
    }

    #[test]
    fn generated_verifiers_are_unpadded_base64url_and_unique() {
        let a = Pkce::generate();
        let b = Pkce::generate();
        assert_ne!(a.verifier(), b.verifier());
        assert_eq!(a.verifier().len(), 43, "32 bytes base64url is 43 chars");
        for pkce in [&a, &b] {
            assert!(
                pkce.verifier()
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{} is not base64url",
                pkce.verifier()
            );
            assert_eq!(pkce.challenge(), challenge_for(pkce.verifier()));
        }
    }

    #[test]
    fn debug_never_reveals_the_verifier_or_the_code() {
        let pkce = Pkce::from_verifier("verifier-supersecret");
        let rendered = format!("{pkce:?}");
        assert!(!rendered.contains("supersecret"), "{rendered}");
        assert!(rendered.contains(pkce.challenge()));

        let callback = Callback {
            code: "code-supersecret".into(),
            state: "state-supersecret".into(),
        };
        assert!(!format!("{callback:?}").contains("supersecret"));
    }

    #[tokio::test]
    async fn serves_one_callback_then_stops() {
        let mut waiter = CodeWaiter::bind(0, "/auth/callback").await.unwrap();
        let base = format!("http://{}", waiter.local_addr());

        // A request on another path is a 404 and does not end the wait.
        let stray = reqwest::get(format!("{base}/favicon.ico")).await.unwrap();
        assert_eq!(stray.status(), reqwest::StatusCode::NOT_FOUND);

        let hit = reqwest::get(format!("{base}/auth/callback?code=abc123&state=xyz"))
            .await
            .unwrap();
        assert!(hit.status().is_success());
        assert!(hit.text().await.unwrap().contains("Signed in"));

        let callback = waiter.recv().await.unwrap();
        assert_eq!(callback.code, "abc123");
        assert_eq!(callback.state, "xyz");
    }

    #[tokio::test]
    async fn an_error_redirect_fails_the_wait() {
        let mut waiter = CodeWaiter::bind(0, "/callback").await.unwrap();
        let base = format!("http://{}", waiter.local_addr());

        let hit = reqwest::get(format!(
            "{base}/callback?error=access_denied&error_description=user%20said%20no"
        ))
        .await
        .unwrap();
        assert_eq!(hit.status(), reqwest::StatusCode::BAD_REQUEST);

        let err = waiter.recv().await.unwrap_err();
        assert!(err.to_string().contains("user said no"), "{err}");
    }

    #[tokio::test]
    async fn binding_a_port_twice_is_a_clear_error_not_a_panic() {
        let first = CodeWaiter::bind(0, "/callback").await.unwrap();
        let port = first.local_addr().port();
        let err = CodeWaiter::bind(port, "/callback").await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains(&port.to_string()), "{message}");
    }
}
