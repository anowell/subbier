//! `subbier` — the headless frontend over `libsubby`. Every subcommand is a
//! thin projection of a snapshot; nothing here computes a number. Only `serve`
//! binds the proxy port, since the rest run *while* the menu bar app does.
//! stdout carries the answer and stderr the noise, so both are parseable.

#![forbid(unsafe_code)]

mod codex_setup;
mod envcmd;
mod login;
mod report;
mod runtime;
mod serve;
mod service;
mod status;
mod style;
mod subs;
mod tui;
mod watch;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use libsubby::Provider;

/// Ends up on stderr, prefixed with `subbier: `.
pub type Error = Box<dyn std::error::Error>;
pub type Result<T = ()> = std::result::Result<T, Error>;

/// Watch your Claude and Codex subscription usage, and run the balancing proxy.
#[derive(Debug, Parser)]
#[command(name = "subbier", version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,
    #[command(subcommand)]
    command: Command,
}

/// Flags every subcommand accepts, before or after the subcommand name.
#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    /// Path to `config.kdl` (default: `$SUBBIER_HOME/config.kdl`, else
    /// `~/.subbier/config.kdl`).
    #[arg(long, short = 'c', global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Log more: `-v` for debug, `-vv` for trace. Logs go to stderr.
    #[arg(long, short = 'v', global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show usage bars for every account.
    Status(status::StatusArgs),
    /// Print the shell snippet that points `claude` — and OpenAI-compatible
    /// clients — at the proxy.
    Env(envcmd::EnvArgs),
    /// Point `codex` at the proxy. A ChatGPT-signed-in `codex` ignores
    /// `OPENAI_BASE_URL`, so this needs `config.toml`, not env vars.
    CodexSetup(codex_setup::CodexSetupArgs),
    /// Add an account beyond the ones `codex` and `claude` are already
    /// logged into.
    Login(login::LoginArgs),
    /// List accounts and where their credentials came from.
    Subs,
    /// Run the proxy headless in the foreground until Ctrl-C.
    Serve,
    /// Install and drive the background subbier (macOS launchd).
    Service(service::ServiceArgs),
    /// Stream a snapshot every time anything changes, for bar widgets.
    Watch(watch::WatchArgs),
}

impl Command {
    /// Only a long-lived process writes a log file and keeps the `info` filter.
    fn long_running(&self) -> bool {
        matches!(self, Command::Serve | Command::Watch(_))
    }

    /// Asked before dispatch: the tracing subscriber is installed once, at startup.
    fn draws_tui(&self) -> bool {
        match self {
            Command::Watch(args) => watch::draws_tui(args),
            _ => false,
        }
    }
}

/// `--shell`: which syntax `subbier env` emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ShellKind {
    /// `export NAME=value` — bash, zsh, dash, ksh.
    Posix,
    /// `set -x NAME value`.
    Fish,
    /// `$env.NAME = "value"`.
    Nushell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProviderArg {
    Codex,
    Claude,
}

impl From<ProviderArg> for Provider {
    fn from(p: ProviderArg) -> Self {
        match p {
            ProviderArg::Codex => Provider::Codex,
            ProviderArg::Claude => Provider::Claude,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let long_running = cli.command.long_running();
    let log_dir = long_running.then(|| libsubby::store::home().join("logs"));
    let filter = log_filter(cli.global.verbose, long_running);
    // A TUI owns the terminal, and `is_terminal()` cannot tell us: stderr IS one.
    let console = if cli.command.draws_tui() {
        libsubby::logging::Console::Silent
    } else {
        libsubby::logging::Console::Stderr
    };
    // Named, because `let _ =` would drop the guard and discard buffered lines.
    let _guard = libsubby::logging::init(log_dir.as_deref(), filter.as_deref(), console);

    let outcome = match cli.command {
        Command::Status(args) => status::run(&cli.global, &args).await,
        Command::Env(args) => envcmd::run(&cli.global, &args).await,
        Command::CodexSetup(args) => codex_setup::run(&cli.global, &args).await,
        Command::Login(args) => login::run(&cli.global, &args).await,
        Command::Subs => subs::run(&cli.global).await,
        Command::Serve => serve::run(&cli.global).await,
        Command::Service(args) => service::run(&cli.global, &args).await,
        Command::Watch(args) => watch::run(&cli.global, &args).await,
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("subbier: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `None` lets `logging::init` use `SUBBIER_LOG` or its own default.
fn log_filter(verbose: u8, long_running: bool) -> Option<String> {
    match verbose {
        0 if long_running => None,
        // A one-shot must not narrate engine startup over its own answer.
        0 => std::env::var(libsubby::logging::FILTER_ENV)
            .is_err()
            .then(|| "subbier=warn,libsubby=warn".to_owned()),
        1 => Some("subbier=debug,libsubby=debug,tower_http=info".to_owned()),
        _ => Some("subbier=trace,libsubby=trace,tower_http=debug".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn the_command_line_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn global_flags_are_accepted_on_either_side_of_the_subcommand() {
        let before = Cli::try_parse_from(["subbier", "--config", "/tmp/c.kdl", "-vv", "status"])
            .expect("flags before the subcommand");
        let after = Cli::try_parse_from(["subbier", "status", "--config", "/tmp/c.kdl", "-vv"])
            .expect("flags after the subcommand");
        assert_eq!(before.global.config, after.global.config);
        assert_eq!(before.global.verbose, 2);
        assert_eq!(after.global.verbose, 2);
    }
}
