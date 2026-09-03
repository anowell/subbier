//! `subbier serve` — run the proxy headless, in the foreground. `SIGINT` and
//! `SIGTERM` are handled inside the engine, which drains in flight requests.
//! It refuses to start next to another subbier: the engine would otherwise
//! carry on unbound, polling a second time into the same `state.db`.

use libsubby::Snapshot;

use crate::runtime::Role;
use crate::{GlobalArgs, Result, runtime};

pub async fn run(global: &GlobalArgs) -> Result {
    let config = runtime::load_config(global)?;
    if let Some(snap) = runtime::probe(&config).await {
        return Err(already_running(&config.proxy.bind, &snap).into());
    }

    let local = runtime::start(global, Role::Server).await?;
    let mut rx = local.handle.subscribe();

    // From the stream, not a guess: with `bind` at port 0 only the bind knows.
    let announcer = tokio::spawn(async move {
        let mut announced = false;
        while rx.changed().await.is_ok() {
            let snap = rx.borrow_and_update().clone();
            if !announced && snap.proxy.listening.is_some() {
                announced = true;
                println!("{}", banner(&snap));
            }
        }
    });

    let outcome = local.wait().await;
    let _ = announcer.await;
    eprintln!("subbier: stopped.");
    Ok(outcome?)
}

fn already_running(bind: &str, snap: &Snapshot) -> String {
    let who = snap.proxy.pid.map_or_else(
        || "a subbier".to_owned(),
        |pid| format!("subbier (pid {pid})"),
    );

    #[cfg(target_os = "macos")]
    let is_the_service = {
        let job = libsubby::service::job_state();
        matches!((job.pid, snap.proxy.pid), (Some(a), Some(b)) if a == b)
    };
    #[cfg(not(target_os = "macos"))]
    let is_the_service = false;

    let advice = if is_the_service {
        "That is the subbier service. Manage it with `subbier service stop`, \
         `subbier service restart`, or `subbier service status`."
    } else {
        "Stop it before running `subbier serve`, or point this one somewhere \
         else with `proxy.bind` in config.kdl."
    };
    format!("{who} is already running on {bind}.\n{advice}")
}

fn banner(snap: &Snapshot) -> String {
    let Some(addr) = snap.proxy.listening else {
        return "subbier: the proxy is not listening".to_owned();
    };
    let base = runtime::url_for(addr);
    // A ChatGPT-signed-in `codex` ignores `OPENAI_BASE_URL`; it is listed here
    // for API-key codex and other OpenAI-compatible clients.
    let mut lines = vec![
        format!("subbier serving on {base}"),
        format!("  claude:  ANTHROPIC_BASE_URL={base}"),
        format!("  openai-compatible clients:  OPENAI_BASE_URL={base}/v1"),
        "  codex:   run `subbier codex-setup` — a ChatGPT-signed-in codex ignores OPENAI_BASE_URL"
            .to_owned(),
    ];
    if snap.proxy.requires_key {
        lines.push("  clients must send proxy.key as a bearer token or x-api-key".to_owned());
    }
    lines.push(format!(
        "{} account(s) · {} · Ctrl-C to stop",
        snap.subs.len(),
        snap.settings.strategy
    ));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use libsubby::snapshot::{ProxyView, SnapshotData};

    use super::*;

    #[test]
    fn refusing_to_double_start_names_the_process_holding_the_port() {
        let with_pid = Snapshot::from(SnapshotData {
            proxy: ProxyView {
                pid: Some(4242),
                ..ProxyView::default()
            },
            ..SnapshotData::default()
        });
        let text = already_running("127.0.0.1:8787", &with_pid);
        assert!(text.contains("pid 4242"), "{text}");
        assert!(text.contains("127.0.0.1:8787"), "{text}");

        let text = already_running("127.0.0.1:8787", &Snapshot::from(SnapshotData::default()));
        assert!(text.starts_with("a subbier is already running"), "{text}");
    }

    #[test]
    fn the_banner_keeps_the_v1_asymmetry_and_sends_codex_to_codex_setup() {
        let snap = Snapshot::from(SnapshotData {
            generation: 2,
            proxy: ProxyView {
                running: true,
                listening: Some("127.0.0.1:8787".parse().unwrap()),
                ..ProxyView::default()
            },
            ..SnapshotData::default()
        });
        let text = banner(&snap);
        assert!(
            text.contains("OPENAI_BASE_URL=http://127.0.0.1:8787/v1"),
            "{text}"
        );
        assert!(
            text.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:8787\n"),
            "{text}"
        );
        assert!(text.contains("subbier codex-setup"), "{text}");
    }
}
