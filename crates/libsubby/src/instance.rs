//! Finding the subbier that is already running. One process owns the proxy port,
//! and the port is how you find it: anything needing live state asks
//! `GET /status` before starting an engine of its own, rather than racing for
//! the bind and doubling the load on a rate-limited usage endpoint.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use crate::{Config, Snapshot};

/// How long an already-running subbier gets to answer `GET /status` before the
/// caller concludes nothing is there.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(750);

/// Ask an already-running subbier for its snapshot. Any failure just means "no
/// running instance", never an error the user needs to hear about.
pub async fn probe(config: &Config) -> Option<Snapshot> {
    let addr = reachable_addr(&config.proxy.bind)?;
    // Port 0 means "whatever the OS picked", unguessable from out here.
    if addr.port() == 0 {
        return None;
    }
    let mut request = crate::http::client()
        .get(format!("{}/status", url_for(addr)))
        .timeout(PROBE_TIMEOUT);
    if let Some(key) = &config.proxy.key {
        request = request.header("x-api-key", key);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            tracing::debug!(error = %e, %addr, "no subbier answered /status");
            return None;
        }
    };
    if !response.status().is_success() {
        tracing::debug!(status = %response.status(), "/status refused us");
        return None;
    }
    match response.json::<Snapshot>().await {
        Ok(snap) => Some(snap),
        Err(e) => {
            // `warn`: version skew or a stranger on the port, which otherwise
            // looks exactly like "nothing is there".
            tracing::warn!(error = %e, %addr, "something answered /status but we could not parse it");
            None
        }
    }
}

/// Resolve a configured `bind` string to an address a *client* can dial.
#[must_use]
pub fn reachable_addr(bind: &str) -> Option<SocketAddr> {
    let addr: SocketAddr = bind.parse().ok()?;
    Some(loopback_if_unspecified(addr))
}

/// `0.0.0.0:8787 -> 127.0.0.1:8787`, `[::]:8787 -> [::1]:8787`, everything
/// else unchanged.
#[must_use]
pub fn loopback_if_unspecified(addr: SocketAddr) -> SocketAddr {
    if !addr.ip().is_unspecified() {
        return addr;
    }
    let ip = match addr.ip() {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
    };
    SocketAddr::new(ip, addr.port())
}

/// `http://host:port`, with the brackets IPv6 needs.
#[must_use]
pub fn url_for(addr: SocketAddr) -> String {
    match addr.ip() {
        IpAddr::V6(ip) => format!("http://[{}]:{}", ip, addr.port()),
        IpAddr::V4(ip) => format!("http://{}:{}", ip, addr.port()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_bind_becomes_a_dialable_loopback_address() {
        assert_eq!(
            reachable_addr("0.0.0.0:8787").unwrap(),
            "127.0.0.1:8787".parse().unwrap()
        );
        assert_eq!(
            reachable_addr("[::]:8787").unwrap(),
            "[::1]:8787".parse().unwrap()
        );
        assert_eq!(
            reachable_addr("127.0.0.1:9000").unwrap(),
            "127.0.0.1:9000".parse().unwrap()
        );
        assert_eq!(
            reachable_addr("not-an-address").map(|a| a.to_string()),
            None
        );
    }

    #[test]
    fn urls_bracket_ipv6_and_leave_ipv4_alone() {
        assert_eq!(
            url_for("127.0.0.1:8787".parse().unwrap()),
            "http://127.0.0.1:8787"
        );
        assert_eq!(url_for("[::1]:8787".parse().unwrap()), "http://[::1]:8787");
    }
}
