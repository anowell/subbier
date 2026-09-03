//! The "Copy env snippet" payload.
//!
//! Built from the **live** [`ProxyView`], not the configured bind: if that port was taken
//! and the proxy landed elsewhere, a snippet naming it would point at nothing.

use libsubby::snapshot::{PoolView, ProxyView};

/// The proxy checks no token when `proxy.key` is unset, but the tools need *something*.
const PLACEHOLDER_KEY: &str = "subbier";

/// **Verbatim from `subbier env`** (`subbier-cli/src/envcmd.rs`, `CODEX_CAVEAT`): two
/// wordings of the same caveat is how one of them quietly stops being true.
const CODEX_CAVEAT: &[&str] = &[
    "# NOTE: `codex` signed in with ChatGPT ignores OPENAI_BASE_URL entirely.",
    "# Run `subbier codex-setup` to route it — it needs a config file, not env vars.",
    "# These two still matter for API-key `codex` and other OpenAI-compatible clients.",
];

/// A paste-ready `export` block pointing the CLIs at the running proxy. One port serves
/// both APIs — `codex` appends `/responses`, `claude` appends `/v1/messages`, which is why
/// only the OpenAI URL carries `/v1`. `pool` narrows both onto `/pool/<name>`.
#[must_use]
pub fn snippet_for(proxy: &ProxyView, pool: Option<&PoolView>) -> String {
    let (openai, anthropic, note) = match (&proxy.openai_base_url, &proxy.anthropic_base_url) {
        (Some(openai), Some(anthropic)) => (openai.clone(), anthropic.clone(), ""),
        _ => {
            let bind = proxy.configured_bind;
            (
                format!("http://{bind}/v1"),
                format!("http://{bind}"),
                "# subbier's proxy is not running; this is the configured bind.\n",
            )
        }
    };

    let (openai, anthropic, pool_note) = match pool {
        None => (openai, anthropic, String::new()),
        Some(pool) => (
            pool.openai_base_url
                .clone()
                .unwrap_or_else(|| format!("{anthropic}/pool/{}/v1", pool.name)),
            pool.anthropic_base_url
                .clone()
                .unwrap_or_else(|| format!("{anthropic}/pool/{}", pool.name)),
            format!(
                "# pool {:?}: this shell can only reach the accounts that pool names.\n",
                pool.name
            ),
        ),
    };

    // The snapshot does not carry `proxy.key`, so say what to substitute rather than
    // pasting a token that would be rejected.
    let key = if proxy.requires_key {
        "<your proxy.key>"
    } else {
        PLACEHOLDER_KEY
    };

    let caveat = CODEX_CAVEAT.join("\n");
    format!(
        "{note}{pool_note}{caveat}\n\
         export OPENAI_BASE_URL={openai}\n\
         export OPENAI_API_KEY={key}\n\
         export ANTHROPIC_BASE_URL={anthropic}\n\
         export ANTHROPIC_AUTH_TOKEN={key}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn running_on(port: u16) -> ProxyView {
        ProxyView {
            running: true,
            listening: Some(SocketAddr::from(([127, 0, 0, 1], port))),
            openai_base_url: Some(format!("http://127.0.0.1:{port}/v1")),
            anthropic_base_url: Some(format!("http://127.0.0.1:{port}")),
            ..ProxyView::default()
        }
    }

    fn pool(name: &str, port: u16) -> PoolView {
        PoolView {
            name: name.to_owned(),
            provider: None,
            members: Vec::new(),
            eligible: Vec::new(),
            max_session_pct: 100.0,
            max_weekly_pct: 100.0,
            openai_base_url: Some(format!("http://127.0.0.1:{port}/pool/{name}/v1")),
            anthropic_base_url: Some(format!("http://127.0.0.1:{port}/pool/{name}")),
            proxied_in_flight: 0,
            proxied_tokens_1h: 0,
        }
    }

    #[test]
    fn uses_the_live_bound_port_and_only_v1_on_the_openai_url() {
        // The configured bind is 8787; the proxy actually landed on 9999.
        let snippet = snippet_for(&running_on(9999), None);
        assert!(
            snippet.contains("export OPENAI_BASE_URL=http://127.0.0.1:9999/v1"),
            "{snippet}"
        );
        assert!(
            snippet.contains("export ANTHROPIC_BASE_URL=http://127.0.0.1:9999\n"),
            "{snippet}"
        );
        assert!(!snippet.contains("8787"), "{snippet}");
        assert!(!snippet.contains("/pool/"), "{snippet}");
    }

    /// The caveat lines are for the human; `eval` must not see them at all.
    #[test]
    fn every_line_a_shell_acts_on_is_an_export() {
        let snippet = snippet_for(&running_on(8787), None);
        let exports: Vec<&str> = snippet.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(exports.len(), 4, "{snippet}");
        assert!(
            exports.iter().all(|l| l.starts_with("export ")),
            "{snippet}"
        );
        assert!(snippet.ends_with('\n'));
    }

    /// Unhedged, the pair sends people off to debug a proxy `codex` never asked anything of.
    #[test]
    fn the_openai_pair_carries_the_caveat_that_codex_ignores_it() {
        let snippet = snippet_for(&running_on(8787), None);
        let lines: Vec<&str> = snippet.lines().collect();
        let openai = lines
            .iter()
            .position(|l| l.starts_with("export OPENAI_BASE_URL"))
            .expect("the pair is emitted");
        // Immediately above the pair, so it cannot be read apart from it.
        assert_eq!(&lines[openai - CODEX_CAVEAT.len()..openai], CODEX_CAVEAT);
        assert!(lines.iter().any(|l| l.contains("subbier codex-setup")));
        // `ANTHROPIC_BASE_URL` works, so nothing is hedged about it.
        let anthropic = lines
            .iter()
            .position(|l| l.starts_with("export ANTHROPIC_BASE_URL"))
            .expect("the pair is emitted");
        assert!(!lines[anthropic - 1].starts_with('#'), "{snippet}");
    }

    #[test]
    fn a_stopped_proxy_falls_back_to_the_configured_bind_and_says_so() {
        let snippet = snippet_for(&ProxyView::default(), None);
        assert!(
            snippet.starts_with("# subbier's proxy is not running"),
            "{snippet}"
        );
        assert!(snippet.contains("http://127.0.0.1:8787/v1"), "{snippet}");
    }

    #[test]
    fn a_keyed_proxy_asks_for_the_key_rather_than_pasting_a_rejected_token() {
        let proxy = ProxyView {
            requires_key: true,
            ..running_on(8787)
        };
        let snippet = snippet_for(&proxy, None);
        assert!(
            snippet.contains("OPENAI_API_KEY=<your proxy.key>"),
            "{snippet}"
        );
        assert!(!snippet.contains("=subbier"), "{snippet}");
    }

    #[test]
    fn a_pool_snippet_points_at_the_pools_own_urls_and_says_it_is_narrowed() {
        let snippet = snippet_for(&running_on(9999), Some(&pool("moonshot", 9999)));
        assert!(
            snippet.contains("OPENAI_BASE_URL=http://127.0.0.1:9999/pool/moonshot/v1"),
            "{snippet}"
        );
        assert!(
            snippet.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:9999/pool/moonshot\n"),
            "{snippet}"
        );
        // Four exports look identical to the whole-proxy pair; the constraint
        // has to travel with them.
        assert!(snippet.contains("# pool \"moonshot\""), "{snippet}");
    }

    /// Derived URLs must still carry the pool: quietly widening to the whole proxy is the one failure.
    #[test]
    fn a_stopped_proxy_still_produces_a_usable_pool_url() {
        let stopped = PoolView {
            openai_base_url: None,
            anthropic_base_url: None,
            ..pool("moonshot", 0)
        };
        let snippet = snippet_for(&ProxyView::default(), Some(&stopped));
        assert!(snippet.contains("/pool/moonshot/v1"), "{snippet}");
        assert!(snippet.contains("/pool/moonshot\n"), "{snippet}");
    }
}
