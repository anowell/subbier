//! `subbier watch` — the terminal UI, or a stream for something else to read.
//! The streaming modes flush every line (stdout is block-buffered on a pipe)
//! and always run their own engine. The TUI instead prefers a running subbier's
//! `GET /status`: a second engine would double the usage polling.

use std::io::{IsTerminal as _, Write as _};

use crate::runtime::Role;
use crate::style::Style;
use crate::{GlobalArgs, Result, runtime, status, tui};

/// Watch usage live: a terminal UI, or a stream for a bar widget.
#[derive(Debug, Clone, clap::Args)]
pub struct WatchArgs {
    /// Emit newline-delimited JSON instead of the terminal UI.
    #[arg(long)]
    pub json: bool,
    /// Reprint the `status` report on every change instead of the terminal UI.
    #[arg(long, conflicts_with = "json")]
    pub plain: bool,
}

/// `main` asks before it installs the tracing subscriber, `run` to dispatch.
pub fn draws_tui(args: &WatchArgs) -> bool {
    !args.json && !args.plain && std::io::stdout().is_terminal()
}

pub async fn run(global: &GlobalArgs, args: &WatchArgs) -> Result {
    if !draws_tui(args) {
        return stream(global, args).await;
    }
    let local = tui::source(global).await?;
    tui::run(global, local).await
}

async fn stream(global: &GlobalArgs, args: &WatchArgs) -> Result {
    let local = runtime::start(global, Role::Watcher).await?;
    let mut rx = local.handle.subscribe();
    let mut stdout = std::io::stdout();
    let style = Style::auto();
    // Generation 0 is the empty snapshot: it would report no accounts at all.
    let mut last = 0u64;

    loop {
        let snap = rx.borrow_and_update().clone();
        if snap.generation > last {
            last = snap.generation;
            let line = if args.json {
                serde_json::to_string(&snap)?
            } else {
                format!(
                    "{}\n",
                    status::render_status(&snap, status::BAR_WIDTH, style)
                )
            };
            // A closed pipe is how `subbier watch | head` ends, not a failure.
            if writeln!(stdout, "{line}").is_err() || stdout.flush().is_err() {
                break;
            }
        }
        if rx.changed().await.is_err() {
            break;
        }
    }

    local.shutdown().await;
    Ok(())
}
