//! `subbier env` — the copy-paste snippet. `codex` appends `/responses` so
//! `OPENAI_BASE_URL` carries `/v1`; `claude` appends `/v1/messages` so
//! `ANTHROPIC_BASE_URL` must not. A ChatGPT-auth `codex` ignores
//! `OPENAI_BASE_URL` entirely; [`crate::codex_setup`] is the half that works.

use libsubby::Provider;
use libsubby::snapshot::ProxyView;

use crate::{GlobalArgs, ProviderArg, Result, ShellKind, runtime};

/// The clients require *something* even with no `proxy.key`.
const PLACEHOLDER_KEY: &str = "subbier";

/// A `#` comment in every shell this command emits, so `eval` ignores them.
const CODEX_CAVEAT: &[&str] = &[
    "# NOTE: `codex` signed in with ChatGPT ignores OPENAI_BASE_URL entirely.",
    "# Run `subbier codex-setup` to route it — it needs a config file, not env vars.",
    "# These two still matter for API-key `codex` and other OpenAI-compatible clients.",
];

/// Print the shell snippet that points `codex` and `claude` at the proxy.
#[derive(Debug, Clone, clap::Args)]
pub struct EnvArgs {
    /// Shell syntax to emit. `nushell` always exports; `--no-export` has no
    /// meaning there and is ignored.
    #[arg(long, value_enum, default_value = "posix")]
    pub shell: ShellKind,

    /// Emit only one provider's pair of variables.
    #[arg(long, value_enum, value_name = "PROVIDER")]
    pub provider: Option<ProviderArg>,

    /// Emit bare `KEY=value` lines instead of exports.
    #[arg(long)]
    pub no_export: bool,

    /// Point at one pool's base URL rather than the whole proxy, so this shell
    /// can only ever spend that pool's accounts.
    #[arg(long, value_name = "NAME")]
    pub pool: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Urls {
    pub openai: String,
    pub anthropic: String,
}

impl Urls {
    /// The offline spelling of `ProxyHandle::openai_base_url`/`anthropic_base_url`.
    fn from_base(base: &str) -> Self {
        Self {
            openai: format!("{base}/v1"),
            anthropic: base.to_owned(),
        }
    }

    fn in_pool(&self, pool: &str) -> Self {
        let base = self.anthropic.trim_end_matches('/').to_owned();
        Self {
            openai: format!("{base}/pool/{pool}/v1"),
            anthropic: format!("{base}/pool/{pool}"),
        }
    }

    fn from_proxy(proxy: &ProxyView) -> Option<Self> {
        match (&proxy.openai_base_url, &proxy.anthropic_base_url) {
            (Some(openai), Some(anthropic)) => Some(Self {
                openai: openai.clone(),
                anthropic: anthropic.clone(),
            }),
            _ => None,
        }
    }
}

/// Base URLs plus comment lines saying where they came from; the live listener
/// wins over the configured bind. Shared with [`crate::codex_setup`].
pub fn resolve(proxy: &ProxyView) -> (Urls, Vec<String>) {
    if let Some(urls) = Urls::from_proxy(proxy) {
        return (urls, Vec::new());
    }
    // Never fail: a shell profile is read long before subbier is started.
    let addr = runtime::loopback_if_unspecified(proxy.configured_bind);
    let mut notes =
        vec!["# subbier is not running; this is the address configured in config.kdl.".to_owned()];
    if addr.port() == 0 {
        notes.push(
            "# proxy.bind names port 0, so the real port is only known while subbier runs."
                .to_owned(),
        );
    }
    (Urls::from_base(&runtime::url_for(addr)), notes)
}

pub async fn run(global: &GlobalArgs, args: &EnvArgs) -> Result {
    let config = runtime::load_config(global)?;

    // A base URL that 404s would be discovered hours later, from a profile.
    if let Some(pool) = &args.pool
        && config.pool(pool).is_none()
    {
        let known: Vec<&str> = config.pools.iter().map(|p| p.name.as_str()).collect();
        let known = if known.is_empty() {
            "no pools are configured".to_owned()
        } else {
            format!("configured pools: {}", known.join(", "))
        };
        return Err(libsubby::Error::config(format!("unknown pool {pool:?} ({known})")).into());
    }

    let (snap, local) = runtime::observe(global).await?;

    let mut out = String::new();
    for line in render(&snap.proxy, config.proxy.key.as_deref(), args) {
        out.push_str(&line);
        out.push('\n');
    }
    // After the engine work, so no log line can interleave with the snippet.
    print!("{out}");

    // Settings, not addressing: `eval "$(subbier env)"` must not see this.
    for provider in emitted_providers(args.provider) {
        if !snap.settings.proxies(provider) {
            eprintln!(
                "subbier: note: proxy.{} is disabled in config.kdl, so the proxy will refuse {} requests",
                provider.id(),
                provider.display_name()
            );
        }
    }

    if let Some(local) = local {
        local.shutdown().await;
    }
    Ok(())
}

fn emitted_providers(only: Option<ProviderArg>) -> Vec<Provider> {
    match only {
        Some(p) => vec![p.into()],
        None => Provider::ALL.to_vec(),
    }
}

fn render(proxy: &ProxyView, key: Option<&str>, args: &EnvArgs) -> Vec<String> {
    let (urls, mut lines) = resolve(proxy);
    let urls = match &args.pool {
        Some(pool) => {
            lines.push(format!(
                "# pool {pool:?}: this shell can only reach the accounts that pool names."
            ));
            urls.in_pool(pool)
        }
        None => urls,
    };

    let key = key.unwrap_or(PLACEHOLDER_KEY);
    for provider in emitted_providers(args.provider) {
        let (base_var, base_val, key_var) = match provider {
            Provider::Codex => ("OPENAI_BASE_URL", &urls.openai, "OPENAI_API_KEY"),
            Provider::Claude => (
                "ANTHROPIC_BASE_URL",
                &urls.anthropic,
                "ANTHROPIC_AUTH_TOKEN",
            ),
        };
        if provider == Provider::Codex {
            lines.extend(CODEX_CAVEAT.iter().map(|l| (*l).to_owned()));
        }
        lines.push(assignment(args, base_var, base_val));
        lines.push(assignment(args, key_var, key));
    }
    lines
}

fn assignment(args: &EnvArgs, name: &str, value: &str) -> String {
    match args.shell {
        ShellKind::Posix => {
            let value = posix_quote(value);
            if args.no_export {
                format!("{name}={value}")
            } else {
                format!("export {name}={value}")
            }
        }
        ShellKind::Fish => {
            let value = fish_quote(value);
            if args.no_export {
                format!("set {name} {value}")
            } else {
                format!("set -x {name} {value}")
            }
        }
        // Nushell has no unexported form of `$env`.
        ShellKind::Nushell => format!("$env.{name} = {}", nu_quote(value)),
    }
}

/// A URL is; a `proxy.key` with a space or a `$` in it is not.
fn is_bare_safe(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:=@%+,-".contains(c))
}

/// `abc` / `'a b'` / `'it'\''s'`.
fn posix_quote(value: &str) -> String {
    if is_bare_safe(value) {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// `abc` / `'a b'`. Fish escapes inside single quotes; POSIX cannot at all.
fn fish_quote(value: &str) -> String {
    if is_bare_safe(value) {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\\', r"\\").replace('\'', r"\'"))
}

/// Nushell strings are always quoted; bare words there are commands.
fn nu_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', r"\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn bind() -> SocketAddr {
        "127.0.0.1:8787".parse().unwrap()
    }

    pub(super) fn running() -> ProxyView {
        ProxyView {
            running: true,
            listening: Some(bind()),
            openai_base_url: Some("http://127.0.0.1:8787/v1".to_owned()),
            anthropic_base_url: Some("http://127.0.0.1:8787".to_owned()),
            ..ProxyView::default()
        }
    }

    fn args() -> EnvArgs {
        EnvArgs {
            pool: None,
            shell: ShellKind::Posix,
            provider: None,
            no_export: false,
        }
    }

    fn exports(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .filter(|l| !l.starts_with('#'))
            .cloned()
            .collect()
    }

    /// The README quotes these verbatim, and every user's profile has them.
    #[test]
    fn the_default_snippet_is_exactly_the_documented_four_exports() {
        assert_eq!(
            exports(&render(&running(), None, &args())),
            vec![
                "export OPENAI_BASE_URL=http://127.0.0.1:8787/v1",
                "export OPENAI_API_KEY=subbier",
                "export ANTHROPIC_BASE_URL=http://127.0.0.1:8787",
                "export ANTHROPIC_AUTH_TOKEN=subbier",
            ]
        );
    }

    #[test]
    fn the_openai_pair_carries_the_caveat_that_codex_ignores_it() {
        let lines = render(&running(), None, &args());
        let openai = lines
            .iter()
            .position(|l| !l.starts_with('#') && l.contains("OPENAI_BASE_URL"))
            .expect("the pair is emitted");
        // Immediately above the pair, so it cannot be read apart from it.
        let caveat = &lines[openai - CODEX_CAVEAT.len()..openai];
        assert_eq!(caveat, CODEX_CAVEAT);
        assert!(caveat.iter().all(|l| l.starts_with('#')), "{caveat:?}");
        assert!(
            lines.iter().any(|l| l.contains("subbier codex-setup")),
            "{lines:?}"
        );
        // ANTHROPIC_BASE_URL works, so nothing is hedged about it.
        let anthropic = lines
            .iter()
            .position(|l| !l.starts_with('#') && l.contains("ANTHROPIC_BASE_URL"))
            .expect("the pair is emitted");
        assert!(!lines[anthropic - 1].starts_with('#'), "{lines:?}");
    }

    #[test]
    fn the_caveat_is_a_comment_in_every_shell_we_emit() {
        for shell in [ShellKind::Posix, ShellKind::Fish, ShellKind::Nushell] {
            let lines = render(&running(), None, &EnvArgs { shell, ..args() });
            assert!(
                lines
                    .iter()
                    .all(|l| l.starts_with('#') || !l.contains("ignores")),
                "{shell:?}: {lines:?}"
            );
            assert!(
                lines.contains(&CODEX_CAVEAT[0].to_owned()),
                "{shell:?}: {lines:?}"
            );
        }
    }

    /// `codex` appends `/responses`; `claude` appends `/v1/messages`.
    #[test]
    fn the_v1_asymmetry_is_pinned() {
        let urls = Urls::from_base("http://127.0.0.1:8787");
        assert_eq!(urls.openai, "http://127.0.0.1:8787/v1");
        assert_eq!(urls.anthropic, "http://127.0.0.1:8787");
    }

    /// `bind` said port 0; the OS chose 49812. Printing `:0` is the bug.
    #[test]
    fn the_live_bound_port_wins_over_the_configured_one() {
        let proxy = ProxyView {
            running: true,
            configured_bind: "127.0.0.1:0".parse().unwrap(),
            listening: Some("127.0.0.1:49812".parse().unwrap()),
            openai_base_url: Some("http://127.0.0.1:49812/v1".to_owned()),
            anthropic_base_url: Some("http://127.0.0.1:49812".to_owned()),
            ..ProxyView::default()
        };
        let lines = render(&proxy, None, &args());
        assert!(lines.iter().all(|l| !l.contains(":0")), "{lines:?}");
        assert!(lines.contains(&"export OPENAI_BASE_URL=http://127.0.0.1:49812/v1".to_owned()));
    }

    #[test]
    fn a_stopped_proxy_warns_in_a_comment_and_still_prints_a_snippet() {
        let proxy = ProxyView {
            configured_bind: bind(),
            ..ProxyView::default()
        };
        let lines = render(&proxy, None, &args());
        assert!(lines[0].starts_with('#'), "{lines:?}");
        assert!(lines[0].contains("not running"));
        assert!(lines.contains(&"export OPENAI_BASE_URL=http://127.0.0.1:8787/v1".to_owned()));
        assert!(lines.contains(&"export ANTHROPIC_BASE_URL=http://127.0.0.1:8787".to_owned()));

        let zero = ProxyView {
            configured_bind: "127.0.0.1:0".parse().unwrap(),
            ..ProxyView::default()
        };
        let lines = render(&zero, None, &args());
        assert!(lines.iter().any(|l| l.contains("port 0")), "{lines:?}");
    }

    #[test]
    fn a_wildcard_bind_is_printed_as_a_loopback_url() {
        let proxy = ProxyView {
            configured_bind: "0.0.0.0:8787".parse().unwrap(),
            ..ProxyView::default()
        };
        let lines = render(&proxy, None, &args());
        assert!(lines.iter().all(|l| !l.contains("0.0.0.0")), "{lines:?}");
        assert!(lines.contains(&"export ANTHROPIC_BASE_URL=http://127.0.0.1:8787".to_owned()));
    }

    #[test]
    fn each_shell_dialect_spells_the_assignment_its_own_way() {
        let fish = EnvArgs {
            shell: ShellKind::Fish,
            ..args()
        };
        assert_eq!(
            exports(&render(&running(), None, &fish)),
            vec![
                "set -x OPENAI_BASE_URL http://127.0.0.1:8787/v1",
                "set -x OPENAI_API_KEY subbier",
                "set -x ANTHROPIC_BASE_URL http://127.0.0.1:8787",
                "set -x ANTHROPIC_AUTH_TOKEN subbier",
            ]
        );

        let bare = EnvArgs {
            no_export: true,
            ..args()
        };
        assert_eq!(
            exports(&render(&running(), None, &bare)),
            vec![
                "OPENAI_BASE_URL=http://127.0.0.1:8787/v1",
                "OPENAI_API_KEY=subbier",
                "ANTHROPIC_BASE_URL=http://127.0.0.1:8787",
                "ANTHROPIC_AUTH_TOKEN=subbier",
            ]
        );

        let bare_fish = EnvArgs {
            shell: ShellKind::Fish,
            no_export: true,
            ..args()
        };
        assert!(
            exports(&render(&running(), None, &bare_fish))[0].starts_with("set OPENAI_BASE_URL ")
        );

        let nu = EnvArgs {
            shell: ShellKind::Nushell,
            ..args()
        };
        assert_eq!(
            exports(&render(&running(), None, &nu))[0],
            "$env.OPENAI_BASE_URL = \"http://127.0.0.1:8787/v1\""
        );
    }

    #[test]
    fn provider_narrows_to_one_pair() {
        let codex = EnvArgs {
            provider: Some(ProviderArg::Codex),
            ..args()
        };
        assert_eq!(
            exports(&render(&running(), None, &codex)),
            vec![
                "export OPENAI_BASE_URL=http://127.0.0.1:8787/v1",
                "export OPENAI_API_KEY=subbier",
            ]
        );

        let claude = EnvArgs {
            provider: Some(ProviderArg::Claude),
            ..args()
        };
        assert_eq!(
            exports(&render(&running(), None, &claude)),
            vec![
                "export ANTHROPIC_BASE_URL=http://127.0.0.1:8787",
                "export ANTHROPIC_AUTH_TOKEN=subbier",
            ]
        );
        assert!(
            !render(&running(), None, &claude)
                .iter()
                .any(|l| l.starts_with('#'))
        );
    }

    #[test]
    fn a_configured_key_replaces_the_placeholder() {
        let lines = render(&running(), Some("s3cret"), &args());
        assert!(lines.contains(&"export OPENAI_API_KEY=s3cret".to_owned()));
        assert!(lines.contains(&"export ANTHROPIC_AUTH_TOKEN=s3cret".to_owned()));
        // Only from lines a shell acts on: the caveat names `subbier codex-setup`.
        assert!(!exports(&lines).iter().any(|l| l.contains(PLACEHOLDER_KEY)));
    }

    /// A key is arbitrary text: unquoted, `it's a key` would end the command.
    #[test]
    fn an_awkward_key_survives_the_round_trip_into_a_shell() {
        assert_eq!(posix_quote("it's a key"), r"'it'\''s a key'");
        assert_eq!(fish_quote("it's a key"), r"'it\'s a key'");
        assert_eq!(fish_quote(r"back\slash"), r"'back\\slash'");
        assert_eq!(nu_quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(
            posix_quote("http://127.0.0.1:8787/v1"),
            "http://127.0.0.1:8787/v1"
        );
        assert_eq!(fish_quote("subbier"), "subbier");
        assert!(!is_bare_safe(""));
    }
}

#[cfg(test)]
mod pool_env_tests {
    use super::tests::running;
    use super::*;

    fn args(pool: Option<&str>) -> EnvArgs {
        EnvArgs {
            shell: ShellKind::Posix,
            provider: None,
            no_export: false,
            pool: pool.map(str::to_owned),
        }
    }

    #[test]
    fn a_pool_moves_both_urls_onto_its_path() {
        let lines = render(&running(), None, &args(Some("moonshot")));
        let text = lines.join("\n");
        assert!(
            text.contains("OPENAI_BASE_URL=http://127.0.0.1:8787/pool/moonshot/v1"),
            "{text}"
        );
        // `claude` appends `/v1/messages` itself, so this one keeps no `/v1`.
        assert!(
            text.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:8787/pool/moonshot\n")
                || text.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:8787/pool/moonshot\""),
            "{text}"
        );
        assert!(!text.contains("/pool/moonshot/v1/messages"), "{text}");
    }

    /// The variables are inert-looking; the constraint has to be visible.
    #[test]
    fn the_snippet_says_which_pool_it_narrows_to() {
        let text = render(&running(), None, &args(Some("moonshot"))).join("\n");
        assert!(text.contains("# pool \"moonshot\""), "{text}");
    }
}
