//! The one outbound HTTP client, so there is a single connection, DNS and TLS
//! session pool. HTTP/1.1 only, deliberately: h2 multiplexes every request onto
//! one connection, so an edge tearing it down mid-upload failed every in-flight
//! request at once. A socket per request confines the fault to one.

use std::sync::OnceLock;
use std::time::Duration;

use reqwest::Client;

const USER_AGENT: &str = concat!("subbier/", env!("CARGO_PKG_VERSION"));

static CLIENT: OnceLock<Client> = OnceLock::new();

/// The shared client. Cheap to call; the first caller builds it.
///
/// No request timeout is set: a streamed proxy response may legitimately run
/// for minutes, so deadlines belong to the caller.
///
/// Panics if the TLS backend cannot be constructed at all.
pub fn client() -> &'static Client {
    CLIENT.get_or_init(build)
}

fn build() -> Client {
    install_crypto_provider();
    Client::builder()
        .user_agent(USER_AGENT)
        .http1_only()
        .connect_timeout(Duration::from_secs(10))
        // Cloudflare closes an idle socket at around 400s; stay well under it.
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(32)
        .tcp_keepalive(Duration::from_secs(60))
        .tcp_nodelay(true)
        .build()
        .expect("building the shared reqwest client")
}

/// The workspace pins `rustls-no-provider`, so without this the first handshake
/// fails at runtime while compiling fine. `Err` means a benign install race.
fn install_crypto_provider() {
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!("a rustls CryptoProvider was already installed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a real round trip proves a crypto provider was installed.
    #[tokio::test]
    #[ignore = "hits the network"]
    async fn a_real_https_get_succeeds() {
        let response = client()
            .get("https://example.com/")
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .expect("TLS handshake failed — is the ring CryptoProvider installed?");
        assert!(response.status().is_success(), "{}", response.status());
    }
}
