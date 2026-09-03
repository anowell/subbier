//! `libsubby` — the whole of subbier minus pixels.
//!
//! Discovers your already-logged-in Codex and Claude accounts ("subs"), polls
//! each one's rate-limit usage, and runs a local HTTP proxy that spreads traffic
//! across them. A frontend reads an immutable [`Snapshot`] and sends a
//! [`Command`], both behind a cheaply-cloneable [`Handle`]; commands never
//! return a value, results appear in the next `Snapshot`. Nothing here knows
//! what a menu or a terminal is, and every type is `Send`. See
//! `docs/ARCHITECTURE.md` for the design.
//!
//! ```no_run
//! # async fn f() -> libsubby::Result<()> {
//! let (engine, handle) = libsubby::engine::Engine::new().await?;
//! tokio::spawn(engine.run());
//!
//! let mut snapshots = handle.subscribe();
//! while snapshots.changed().await.is_ok() {
//!     let snap = snapshots.borrow_and_update().clone();
//!     // render `snap`; send a Command back through `handle` on a click.
//! }
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod auth;
pub mod balance;
pub mod config;
pub mod engine;
pub mod error;
pub mod history;
pub mod http;
pub mod instance;
pub mod logging;
pub mod model;
pub mod pace;
pub mod plan;
pub mod provider;
pub mod proxy;
pub mod render;
#[cfg(target_os = "macos")]
pub mod service;
pub mod severity;
pub mod snapshot;
pub mod store;
pub mod usage;

pub use config::Config;
pub use engine::Engine;
pub use error::{Error, Result};
pub use model::{
    CredentialSource, Credentials, MenuBarStyle, Projection, Provider, Severity, StrategyKind, Sub,
    SubId, SubKey, Tokens, Usage, UsageWindow, WindowKind,
};
pub use snapshot::{Command, Handle, Publisher, Snapshot, SnapshotData};
