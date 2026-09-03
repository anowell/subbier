//! Tracing setup. Each binary calls [`init`] exactly once, at startup.

use std::io::IsTerminal;
use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

pub const DEFAULT_FILTER: &str = "subbier=info,libsubby=info,tower_http=warn";

pub const FILTER_ENV: &str = "SUBBIER_LOG";

const LOG_FILE_PREFIX: &str = "subbier.log";

/// Whether log lines may go to stderr. Not inferrable from `is_terminal()`: the
/// case that must stay silent is a TUI drawing on that same terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    Stderr,
    /// The file layer still records everything.
    Silent,
}

/// Install the process-wide tracing subscriber. `filter` wins over
/// [`FILTER_ENV`], which wins over [`DEFAULT_FILTER`]. Calling this twice keeps
/// the first subscriber and warns rather than panicking.
///
/// The returned guard must be bound to a *named* variable: `let _ = init(..)`
/// drops it immediately, stopping the writer thread before anything flushes,
/// and `#[must_use]` cannot catch that spelling.
///
/// ```no_run
/// # use std::path::Path;
/// # use libsubby::logging::Console;
/// let _guard = libsubby::logging::init(Some(Path::new("/tmp/subbier")), None, Console::Stderr);
/// ```
#[must_use = "dropping the WorkerGuard silently discards every buffered log line"]
pub fn init(dir: Option<&Path>, filter: Option<&str>, console: Console) -> WorkerGuard {
    let env_filter = match filter {
        Some(directives) => EnvFilter::new(directives),
        None => {
            EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
        }
    };

    let (file_layer, guard) = match dir {
        Some(dir) => {
            let _ = std::fs::create_dir_all(dir);
            let appender = tracing_appender::rolling::daily(dir, LOG_FILE_PREFIX);
            let (writer, guard) = tracing_appender::non_blocking(appender);
            (
                Some(fmt::layer().with_ansi(false).with_writer(writer)),
                guard,
            )
        }
        None => {
            let (writer, guard) = tracing_appender::non_blocking(std::io::sink());
            drop(writer);
            (None, guard)
        }
    };

    let stderr_layer = (console == Console::Stderr).then(|| {
        fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(std::io::stderr().is_terminal())
    });

    if tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .try_init()
        .is_err()
    {
        eprintln!("subbier: a tracing subscriber was already installed; keeping the first one");
    }

    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_writes_a_rolling_file_and_is_safe_to_call_twice() {
        let dir = std::env::temp_dir().join(format!("libsubby-logging-{}", std::process::id()));
        let first = init(Some(&dir), Some("libsubby=trace"), Console::Stderr);
        let second = init(None, None, Console::Silent);

        tracing::info!(test = true, "hello from the logging test");
        drop(second);
        drop(first); // flushes

        let wrote_something = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with(LOG_FILE_PREFIX))
            })
            .unwrap_or(false);
        assert!(
            wrote_something,
            "no rolling log file was created in {}",
            dir.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
