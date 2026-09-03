//! `subbier login <codex|claude>` — add an account by OAuth, as a loop over
//! snapshots. The URL goes to stdout even when the browser opened, for ssh.
//! Ctrl-C is ours here and only here: engine shutdown clears `snapshot.login`,
//! the same change a *successful* login makes, so it would report success.

use std::collections::HashSet;
use std::time::Duration;

use libsubby::snapshot::LoginState;
use libsubby::{Command, Provider, Snapshot, SubId};

use crate::runtime::Role;
use crate::{GlobalArgs, ProviderArg, Result, runtime};

/// Generous: the round trip includes a provider login and possibly 2FA.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Add an account beyond the ones `codex` and `claude` are already logged into.
#[derive(Debug, Clone, clap::Args)]
pub struct LoginArgs {
    /// Which provider to log into.
    #[arg(value_enum)]
    pub provider: ProviderArg,
}

pub async fn run(global: &GlobalArgs, args: &LoginArgs) -> Result {
    let provider: Provider = args.provider.into();

    // `subs.json` has one writer: the running instance would drop this account.
    let config = runtime::load_config(global)?;
    if runtime::probe(&config).await.is_some() {
        return Err(format!(
            "another subbier is already running on {}, and it owns subs.json.\n\
             Log in from that instance (the menu bar's Log in…), or stop it first.",
            config.proxy.bind
        )
        .into());
    }

    let local = runtime::start(global, Role::Interactive).await?;
    let mut rx = local.handle.subscribe();
    let before: HashSet<SubId> = local
        .first_snapshot()
        .await
        .subs
        .iter()
        .map(|s| s.id)
        .collect();

    local.handle.send(Command::Login(provider));

    let outcome = tokio::time::timeout(LOGIN_TIMEOUT, watch_login(&mut rx))
        .await
        .unwrap_or(Outcome::TimedOut);

    let result = match outcome {
        Outcome::Done(snap) => {
            report_added(&snap, &before, provider);
            Ok(())
        }
        Outcome::Failed(error) => Err(format!("login failed: {error}").into()),
        Outcome::EngineGone => Err("the engine stopped before the login finished".into()),
        Outcome::Cancelled => {
            local.handle.send(Command::CancelLogin);
            Err("login cancelled".into())
        }
        Outcome::TimedOut => {
            local.handle.send(Command::CancelLogin);
            Err(format!(
                "timed out after {}s waiting for the browser callback",
                LOGIN_TIMEOUT.as_secs()
            )
            .into())
        }
    };

    // Flushes any pending `subs.json` write, so a new account survives.
    local.shutdown().await;
    result
}

async fn watch_login(rx: &mut tokio::sync::watch::Receiver<Snapshot>) -> Outcome {
    let mut announced = false;
    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return Outcome::EngineGone;
                }
            }
            // Ours, because the engine's own handler would look like success.
            _ = tokio::signal::ctrl_c() => return Outcome::Cancelled,
        }

        let snap = rx.borrow_and_update().clone();
        match &snap.login {
            Some(LoginState::AwaitingBrowser { url, .. }) if !announced => {
                announced = true;
                println!("Open this URL to authorize subbier:");
                println!("{url}");
                println!("Waiting for the callback…");
            }
            Some(LoginState::AwaitingBrowser { .. }) => {}
            Some(LoginState::Failed { error, .. }) => return Outcome::Failed(error.clone()),
            // Cleared means finished, but it is also `None` before the command lands.
            None if announced => return Outcome::Done(snap),
            None => {}
        }
    }
}

enum Outcome {
    Done(Snapshot),
    Failed(String),
    EngineGone,
    Cancelled,
    TimedOut,
}

fn report_added(snap: &Snapshot, before: &HashSet<SubId>, provider: Provider) {
    match snap.subs.iter().find(|s| !before.contains(&s.id)) {
        Some(sub) => println!("Added {} account {}.", sub.provider.id(), sub.label),
        // A re-login refreshes credentials in place, adding no row.
        None => println!("{provider} login completed; credentials updated."),
    }
}
