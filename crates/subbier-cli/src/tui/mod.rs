//! `subbier watch`'s terminal UI: the live report left, `state.db` charts right.
//! Live numbers come from whoever owns the proxy; the history pane reads
//! `state.db` directly, which sqlite's WAL allows while another process writes.
//! [`run`] restores the terminal and its title on every exit path.

mod app;
mod draw;

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use libsubby::history::{AllowanceHistory, Throughput};
use libsubby::store::db::Db;
use libsubby::{Config, Snapshot};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::SetTitle;
use tokio::sync::mpsc;

use crate::runtime::{self, Role};
use crate::{GlobalArgs, Result};

use app::{App, Range};

const LIVE_POLL: Duration = Duration::from_millis(1000);

/// Set by how soon a burst of traffic should appear, not by the write rate.
const HISTORY_POLL: Duration = Duration::from_secs(5);

/// Without this the host's fallback title is `Pane #2` or the shell's name.
const TITLE: &str = "subbier";

/// Redraw even with nothing to react to: the chart's x axis ends at "now".
const TICK: Duration = Duration::from_millis(500);

enum Update {
    Key(KeyEvent),
    Live(Box<std::result::Result<Snapshot, String>>),
    History(Box<(AllowanceHistory, Throughput)>),
    Resize,
}

pub(crate) async fn run(global: &GlobalArgs, local: Option<runtime::Local>) -> Result {
    let (tx, mut rx) = mpsc::channel::<Update>(64);
    let config = runtime::load_config(global)?;

    let first = spawn_live(&tx, config.clone(), local.as_ref());
    let mut app = App::new(first.await);

    // Each range has its own bucket width, so it is a different query.
    let (range_tx, range_rx) = tokio::sync::watch::channel(app.range);
    spawn_history(&tx, range_rx, config.history.retain_days);
    spawn_keys(&tx);

    let mut terminal = ratatui::init();
    set_title(TITLE);
    let result = event_loop(&mut terminal, &mut app, &mut rx, &range_tx).await;
    ratatui::restore();
    // Empty, not `TITLE`: the host's fallback is at least true once we exit.
    set_title("");

    if let Some(local) = local {
        local.shutdown().await;
    }
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &mut mpsc::Receiver<Update>,
    range_tx: &tokio::sync::watch::Sender<Range>,
) -> Result {
    loop {
        terminal.draw(|frame| draw::draw(frame, app))?;
        if app.quit {
            return Ok(());
        }
        match tokio::time::timeout(TICK, rx.recv()).await {
            Err(_) | Ok(Some(Update::Resize)) => {}
            Ok(None) => return Ok(()),
            Ok(Some(Update::Key(key))) => {
                on_key(app, key);
                // An unchanged range must not re-query: `j` is not a sqlite trip.
                range_tx.send_if_modified(|current| {
                    let changed = *current != app.range;
                    *current = app.range;
                    changed
                });
            }
            Ok(Some(Update::Live(result))) => match *result {
                Ok(snap) => app.set_snapshot(snap),
                Err(error) => app.feed_error = Some(error),
            },
            Ok(Some(Update::History(history))) => {
                let (allowance, throughput) = *history;
                app.allowance = allowance;
                app.throughput = throughput;
            }
        }
    }
}

/// The cursor walks accounts only; an endpoint has no allowance to chart, so
/// `p` round-robins them instead.
fn on_key(app: &mut App, key: KeyEvent) {
    // Windows sends a Release for every Press; acting on both double-steps.
    if key.kind != KeyEventKind::Press {
        return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.quit();
        return;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit(),
        KeyCode::Char('j') | KeyCode::Down => app.cursor_down(),
        KeyCode::Char('k') | KeyCode::Up => app.cursor_up(),
        KeyCode::Char(' ') => app.toggle(),
        KeyCode::Char('a') => app.toggle_all(),
        KeyCode::Char('p') => app.next_endpoint(),
        KeyCode::Tab | KeyCode::BackTab => app.next_page(),
        KeyCode::Char(']') | KeyCode::Right => app.wider(),
        KeyCode::Char('[') | KeyCode::Left => app.narrower(),
        // `0` is the mixed range, so the index is the digit itself.
        KeyCode::Char(c @ '0'..='5') => {
            app.set_range_index(c.to_digit(10).unwrap_or(0) as usize);
        }
        _ => {}
    }
}

/// Best effort: a failed write is no reason to refuse to draw a chart.
fn set_title(title: &str) {
    use std::io::Write as _;
    let mut stdout = std::io::stdout();
    let _ = ratatui::crossterm::execute!(stdout, SetTitle(title));
    let _ = stdout.flush();
}

/// Two shapes behind one channel — polling a running subbier's `/status`, or
/// subscribing to our own engine — which the app cannot tell apart.
fn spawn_live(
    tx: &mpsc::Sender<Update>,
    config: Config,
    local: Option<&runtime::Local>,
) -> impl std::future::Future<Output = Snapshot> + 'static {
    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
    let tx = tx.clone();

    match local {
        None => {
            tokio::spawn(async move {
                let mut first = Some(first_tx);
                let mut ticker = tokio::time::interval(LIVE_POLL);
                loop {
                    let update = match runtime::probe(&config).await {
                        Some(snap) => {
                            if let Some(first) = first.take() {
                                let _ = first.send(snap.clone());
                            }
                            Ok(snap)
                        }
                        // Keep the last snapshot up and say why it is stale.
                        None => Err("no subbier is answering /status".to_owned()),
                    };
                    if tx.send(Update::Live(Box::new(update))).await.is_err() {
                        return;
                    }
                    ticker.tick().await;
                }
            });
        }
        Some(local) => {
            let handle = local.handle.clone();
            tokio::spawn(async move {
                let mut rx = handle.subscribe();
                let mut first = Some(first_tx);
                loop {
                    let snap = rx.borrow_and_update().clone();
                    if !snap.is_empty() {
                        if let Some(first) = first.take() {
                            let _ = first.send(snap.clone());
                        }
                        if tx.send(Update::Live(Box::new(Ok(snap)))).await.is_err() {
                            return;
                        }
                    }
                    if rx.changed().await.is_err() {
                        return;
                    }
                }
            });
        }
    }

    async move { first_rx.await.unwrap_or_else(|_| Snapshot::empty()) }
}

/// On our own handle, since another process usually owns the history. A database
/// we cannot open leaves the pane empty; it never fails the command.
fn spawn_history(
    tx: &mpsc::Sender<Update>,
    mut range: tokio::sync::watch::Receiver<Range>,
    retain_days: u32,
) {
    let path = libsubby::store::home().join("state.db");
    let tx = tx.clone();
    tokio::spawn(async move {
        let db = match Db::open(&path, retain_days) {
            Ok(db) => Arc::new(db),
            Err(e) => {
                tracing::warn!(error = %e, "no state.db; the history pane will stay empty");
                return;
            }
        };
        let mut ticker = tokio::time::interval(HISTORY_POLL);
        loop {
            // Per pass, so a query is always for the charts on screen now.
            let current = *range.borrow_and_update();
            let now = Timestamp::now();
            // Allowance rows come back unbucketed and are resampled per chart,
            // so one query serves them all; throughput is summed in sqlite.
            let allowance = db
                .allowance_since(now - current.allowance_span())
                .await
                .map(AllowanceHistory::from_rows)
                .unwrap_or_default();
            let throughput_window = current.throughput_window();
            let throughput = db
                .proxied_since(now - throughput_window.span, throughput_window.bucket)
                .await
                .map(|rows| Throughput::from_buckets(rows, throughput_window.bucket))
                .unwrap_or_default();
            if tx
                .send(Update::History(Box::new((allowance, throughput))))
                .await
                .is_err()
            {
                return;
            }
            // A range change redraws at once, not `HISTORY_POLL` later.
            tokio::select! {
                _ = ticker.tick() => {}
                changed = range.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    });
}

/// A plain `std::thread`, **not** `spawn_blocking`: dropping a tokio runtime
/// waits for its blocking pool, and this one is parked in `event::read`.
fn spawn_keys(tx: &mpsc::Sender<Update>) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        loop {
            let update = match event::read() {
                Ok(Event::Key(key)) => Update::Key(key),
                Ok(Event::Resize(_, _)) => Update::Resize,
                Ok(_) => continue,
                Err(_) => return,
            };
            if tx.blocking_send(update).is_err() {
                return;
            }
        }
    });
}

/// `None` when a running subbier can answer instead.
pub(crate) async fn source(global: &GlobalArgs) -> Result<Option<runtime::Local>> {
    let config = runtime::load_config(global)?;
    if runtime::probe(&config).await.is_some() {
        return Ok(None);
    }
    Ok(Some(runtime::start(global, Role::Watcher).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventState;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn quit_keys_quit_and_only_presses_are_acted_on() {
        for quit in [
            key(KeyCode::Char('q')),
            KeyEvent {
                modifiers: KeyModifiers::CONTROL,
                ..key(KeyCode::Char('c'))
            },
        ] {
            let mut app = App::new(Snapshot::empty());
            on_key(&mut app, quit);
            assert!(app.quit, "{quit:?}");
        }

        // Windows sends a Release for every Press.
        let mut app = App::new(Snapshot::empty());
        on_key(
            &mut app,
            KeyEvent {
                kind: KeyEventKind::Release,
                ..key(KeyCode::Char('q'))
            },
        );
        assert!(!app.quit);
    }
}
