//! "Launch at login", as a `~/Library/LaunchAgents` plist. Its label, path and body live
//! in [`libsubby::service`], because `subbier service` writes the same file. This path
//! touches the file only: `launchctl` from here would kill the running app or start a
//! second one, so the change takes effect at the next login.

use libsubby::service;

/// Against the filesystem, so a plist deleted behind our back is restored.
pub fn reconcile(wanted: bool) {
    let Some(path) = service::plist_path() else {
        return;
    };
    if path.exists() == wanted {
        return;
    }

    let result = if wanted {
        // `current_exe` is the honest answer to "what should run at login"; an install
        // that wants a different path uses `subbier service`.
        std::env::current_exe()
            .map_err(libsubby::Error::from)
            .and_then(|program| service::install(&program, &[]))
    } else {
        service::remove()
    };
    if let Err(error) = result {
        tracing::warn!(%error, path = %path.display(), wanted, "could not update the launch agent");
    }
}
