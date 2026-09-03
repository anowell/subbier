//! Getting a [`Snapshot`]: one loopback `GET /status` when a subbier is up,
//! otherwise our own engine. Only [`Role::Server`] enables the listener — a
//! one-shot run beside a live subbier would either fail to bind or win the race
//! and leave the menu bar app proxy-less.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use libsubby::snapshot::SubHealth;
use libsubby::store::db::Db;
use libsubby::{Command, Config, Engine, Handle, Snapshot};
use tokio::task::JoinHandle;

use crate::{GlobalArgs, Result};

// Shared with the other frontends, so they live in `libsubby::instance`.
pub use libsubby::instance::{loopback_if_unspecified, probe, url_for};

/// A one-shot renders whatever it has after this: hanging on a flaky provider
/// is worse than saying the numbers are unknown.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// The engine's first publish, the subs before any poll; generation 0 is empty.
const FIRST_PUBLISH: u64 = 1;

/// An engine this process started and is responsible for stopping.
#[derive(Debug)]
pub struct Local {
    pub handle: Handle,
    task: JoinHandle<libsubby::Result<()>>,
}

impl Local {
    /// The engine drains in-flight work and persists pending state on the way out.
    pub async fn shutdown(self) {
        self.handle.send(Command::Shutdown);
        if let Err(e) = self.task.await {
            tracing::warn!(error = %e, "the engine task did not join cleanly");
        }
    }

    /// Wait for the engine to stop on its own, on the Ctrl-C it handles itself.
    pub async fn wait(self) -> libsubby::Result<()> {
        match self.task.await {
            Ok(result) => result,
            Err(e) => {
                tracing::error!(error = %e, "the engine task panicked");
                Ok(())
            }
        }
    }

    /// The "what did we start with" baseline. Not [`Handle::snapshot`] straight
    /// after [`start`], which can still be empty and make every account look new.
    pub async fn first_snapshot(&self) -> Snapshot {
        let mut rx = self.handle.subscribe();
        loop {
            let snap = rx.borrow_and_update().clone();
            if snap.generation >= FIRST_PUBLISH {
                return snap;
            }
            if rx.changed().await.is_err() {
                return self.handle.snapshot();
            }
        }
    }

    /// A snapshot whose allowances have actually been polled, falling back after
    /// [`READY_TIMEOUT`] to `SubHealth::Unknown` rather than a misleading zero.
    pub async fn polled_snapshot(&self) -> Snapshot {
        let mut rx = self.handle.subscribe();
        let wait = async {
            loop {
                let snap = rx.borrow_and_update().clone();
                if is_polled(&snap) {
                    return snap;
                }
                if rx.changed().await.is_err() {
                    return self.handle.snapshot();
                }
            }
        };
        match tokio::time::timeout(READY_TIMEOUT, wait).await {
            Ok(snap) => snap,
            Err(_) => {
                tracing::warn!("timed out waiting for the first usage poll");
                self.handle.snapshot()
            }
        }
    }
}

/// Every sub leaves [`SubHealth::Unknown`] once the first round finishes; the
/// generation is the backstop for a failed first poll, or for having no subs.
fn is_polled(snap: &Snapshot) -> bool {
    if snap.generation < FIRST_PUBLISH {
        return false;
    }
    snap.generation > FIRST_PUBLISH
        || snap
            .subs
            .iter()
            .all(|s| !matches!(s.health, SubHealth::Unknown))
}

pub fn config_path(global: &GlobalArgs) -> PathBuf {
    global
        .config
        .clone()
        .unwrap_or_else(|| libsubby::store::home().join("config.kdl"))
}

/// A missing file is the default config; only an unparsable one is an error.
pub fn load_config(global: &GlobalArgs) -> Result<Config> {
    Ok(Config::load_from(&config_path(global))?)
}

/// The listener, history and signal handling are not independent choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A read-only one-shot: `status`, `env`, `subs`.
    Observer,
    /// `subbier watch` alone: no listener, but its samples are the only ones.
    Watcher,
    /// `subbier login`: **this process** handles Ctrl-C, since engine shutdown
    /// clears `snapshot.login` exactly the way success does.
    Interactive,
    /// `subbier serve`: listener, history, and the engine's own signal handling.
    Server,
}

impl Role {
    const fn serves_proxy(self) -> bool {
        matches!(self, Role::Server)
    }

    /// Not the same question as the listener: a watcher records without serving.
    const fn records_history(self) -> bool {
        matches!(self, Role::Server | Role::Watcher)
    }

    const fn engine_handles_signal(self) -> bool {
        !matches!(self, Role::Interactive)
    }
}

pub async fn start(global: &GlobalArgs, role: Role) -> Result<Local> {
    let home = libsubby::store::ensure_home()?;
    let config_path = config_path(global);
    let db = role
        .records_history()
        .then(|| open_db(&home, &config_path))
        .flatten();

    let (engine, handle) = Engine::builder()
        .config_path(config_path)
        .subs_path(home.join("subs.json"))
        .db(db)
        .serve_proxy(role.serves_proxy())
        .shutdown_on_signal(role.engine_handles_signal())
        .build()
        .await?;

    let task = tokio::spawn(engine.run());
    Ok(Local { handle, task })
}

/// Losing history must not stop the proxy from running.
fn open_db(home: &std::path::Path, config_path: &std::path::Path) -> Option<Arc<Db>> {
    let retain_days = Config::load_from(config_path).map_or(7, |c| c.history.retain_days);
    match Db::open(&home.join("state.db"), retain_days) {
        Ok(db) => Some(Arc::new(db)),
        Err(e) => {
            tracing::warn!(error = %e, "could not open state.db; history is disabled");
            None
        }
    }
}

/// The snapshot a read-only subcommand should render, plus the engine the caller
/// must `shutdown()` — `None` when a running instance answered.
pub async fn observe(global: &GlobalArgs) -> Result<(Snapshot, Option<Local>)> {
    let config = load_config(global)?;
    if let Some(snap) = probe(&config).await {
        tracing::debug!(
            generation = snap.generation,
            "using a running subbier's snapshot"
        );
        return Ok((snap, None));
    }
    let local = start(global, Role::Observer).await?;
    let snap = local.polled_snapshot().await;
    Ok((snap, Some(local)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Snapshot::empty()` renders fine, and would report zeros as facts.
    #[test]
    fn generation_zero_is_never_treated_as_polled() {
        assert!(!is_polled(&Snapshot::empty()));
    }
}
