//! The one error type for `libsubby`.
//!
//! Variants are deliberately coarse: one earns its place either because a
//! caller branches on it or because a foreign error type needs a home.

use std::fmt;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("http error: {}", chain(.0))]
    Http(#[from] reqwest::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("kdl parse error: {0}")]
    Kdl(#[from] kdl::KdlError),

    /// `config.kdl` parsed but did not match the typed config shape.
    #[error("kdl decode error: {0}")]
    KdlDecode(#[from] kdl::de::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("time error: {0}")]
    Time(#[from] jiff::Error),

    #[error("url error: {0}")]
    Url(#[from] url::ParseError),

    /// OAuth, PKCE, token refresh, keychain access.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// The user's configuration is invalid or contradictory.
    #[error("configuration error: {0}")]
    Config(String),

    /// A provider API answered with a non-success status.
    ///
    /// `message` is a truncated body prefix — never a full body, never anything
    /// carrying a token. `retry_after` is set only when `Retry-After` named a
    /// *positive* number of seconds (the Anthropic usage endpoint sends
    /// `retry-after: 0` with a 429), and is a floor under a backoff, not the
    /// backoff.
    #[error("upstream returned {status}: {message}")]
    Upstream {
        status: u16,
        message: String,
        retry_after: Option<std::time::Duration>,
    },

    /// No subscription is usable (none discovered, or all disabled).
    #[error("no usable subscriptions")]
    NoSubs,

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn auth(msg: impl fmt::Display) -> Self {
        Self::Auth(msg.to_string())
    }

    pub fn config(msg: impl fmt::Display) -> Self {
        Self::Config(msg.to_string())
    }

    pub fn upstream(status: u16, message: impl fmt::Display) -> Self {
        Self::upstream_after(status, message, None)
    }

    /// `retry_after` must already be a positive duration; see [`Error::Upstream`].
    pub fn upstream_after(
        status: u16,
        message: impl fmt::Display,
        retry_after: Option<std::time::Duration>,
    ) -> Self {
        Self::Upstream {
            status,
            message: message.to_string(),
            retry_after,
        }
    }

    pub fn other(msg: impl fmt::Display) -> Self {
        Self::Other(msg.to_string())
    }
}

/// An error and its whole `source()` chain, rendered `outer: inner: cause`.
///
/// `reqwest::Error` displays only its kind and URL; what actually happened is
/// one to three `source()` hops down.
#[must_use]
pub fn chain(error: &dyn std::error::Error) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        let text = next.to_string();
        if !out.ends_with(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = next.source();
    }
    out
}

/// `Result` with this crate's [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

// `Error` crosses task boundaries into a Snapshot; keep it thread-safe.
const _: fn() = || {
    fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<Error>();
};
