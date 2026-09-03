//! The macOS LaunchAgent that runs subbier in the background: a
//! `~/Library/LaunchAgents` plist, not `SMAppService`, which returns `NotFound`
//! without a Developer-ID signature. The domain is `gui/<uid>`, not `user/<uid>`:
//! the login Keychain and the menu bar both need the user's GUI session.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::error::{Error, Result};

/// The launch agent's label, its plist basename, and its launchd service name.
pub const LABEL: &str = "com.github.anowell.subbier";

/// `~/Library/LaunchAgents/com.github.anowell.subbier.plist`.
#[must_use]
pub fn plist_path() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")),
    )
}

/// Is the plist on disk? Whether launchd knows of it is [`job_state`].
#[must_use]
pub fn is_installed() -> bool {
    plist_path().is_some_and(|p| p.exists())
}

/// Write the plist, creating `~/Library/LaunchAgents` if needed.
/// `program` must be absolute: launchd searches no `PATH`.
pub fn install(program: &Path, args: &[String]) -> Result<()> {
    if !program.is_absolute() {
        return Err(Error::other(format!(
            "the launch agent needs an absolute path to the program, not {}",
            program.display()
        )));
    }
    let path = plist_path().ok_or_else(|| Error::other("no home directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // launchd redirects the job's stderr before subbier's own logging is up, so
    // a startup refusal ("another subbier owns the port") has somewhere to land.
    let log_dir = crate::store::home().join("logs");
    let _ = std::fs::create_dir_all(&log_dir);

    std::fs::write(&path, plist(program, args, &log_dir))?;
    tracing::info!(path = %path.display(), program = %program.display(), "installed the launch agent");
    Ok(())
}

/// Remove the plist. Removing one that is not there is success, not an error.
pub fn remove() -> Result<()> {
    let path = plist_path().ok_or_else(|| Error::other("no home directory"))?;
    match std::fs::remove_file(&path) {
        Ok(()) => {
            tracing::info!(path = %path.display(), "removed the launch agent");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// The program and arguments the installed plist names, best-effort: the file
/// is ours and generated, so this scans it rather than parsing a property list.
#[must_use]
pub fn installed_program() -> Option<Vec<String>> {
    let text = std::fs::read_to_string(plist_path()?).ok()?;
    let (_, after) = text.split_once("<key>ProgramArguments</key>")?;
    let (_, array) = after.split_once("<array>")?;
    let (array, _) = array.split_once("</array>")?;
    let args: Vec<String> = array
        .split("<string>")
        .skip(1)
        .filter_map(|s| s.split_once("</string>"))
        .map(|(value, _)| unescape(value))
        .collect();
    (!args.is_empty()).then_some(args)
}

/// `KeepAlive` is false on purpose: quitting from the menu means quit, not
/// "restart me immediately". `ProcessType Interactive` keeps the menu
/// responsive under App Nap.
#[must_use]
pub fn plist(program: &Path, args: &[String], log_dir: &Path) -> String {
    let mut program_arguments = format!(
        "\t\t<string>{}</string>\n",
        escape(&program.to_string_lossy())
    );
    for arg in args {
        program_arguments.push_str(&format!("\t\t<string>{}</string>\n", escape(arg)));
    }
    let err_log = log_dir.join("launchd.err.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
{program_arguments}	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<false/>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>StandardErrorPath</key>
	<string>{}</string>
</dict>
</plist>
"#,
        escape(&err_log.to_string_lossy())
    )
}

/// What launchd currently knows about the job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobState {
    pub loaded: bool,
    /// A loaded job with no pid is installed and idle — the state after `stop`.
    pub pid: Option<u32>,
    /// Non-zero after a `start` means it ran and refused; the reason is in
    /// `launchd.err.log`.
    pub last_exit_status: Option<i32>,
}

/// The current user id, cached. Shelled out to `id` because `getuid` has no safe
/// std spelling and this crate forbids `unsafe`.
fn uid() -> Option<u32> {
    static UID: OnceLock<Option<u32>> = OnceLock::new();
    *UID.get_or_init(|| {
        let output = Command::new("id").arg("-u").output().ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().parse().ok())
            .flatten()
    })
}

/// `gui/501` — the domain a login session's agents live in.
pub fn domain() -> Result<String> {
    uid()
        .map(|uid| format!("gui/{uid}"))
        .ok_or_else(|| Error::other("could not determine the current user id"))
}

/// `gui/501/com.github.anowell.subbier` — one job inside that domain.
pub fn target() -> Result<String> {
    Ok(format!("{}/{LABEL}", domain()?))
}

/// Ask launchd about the job. Any failure reads as "not loaded".
#[must_use]
pub fn job_state() -> JobState {
    let Ok(output) = Command::new("launchctl").arg("list").arg(LABEL).output() else {
        return JobState::default();
    };
    if !output.status.success() {
        return JobState::default();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    JobState {
        loaded: true,
        pid: plist_field(&text, "PID").and_then(|v| v.parse().ok()),
        last_exit_status: plist_field(&text, "LastExitStatus").and_then(|v| v.parse().ok()),
    }
}

/// Pull `"<key>" = <value>;` out of `launchctl list`'s plist-ish output.
fn plist_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\" = ");
    let (_, after) = text.split_once(&needle)?;
    let (value, _) = after.split_once(';')?;
    Some(value.trim())
}

/// Load the job into launchd. `RunAtLoad` means this also starts it.
pub fn bootstrap() -> Result<()> {
    let path = plist_path().ok_or_else(|| Error::other("no home directory"))?;
    launchctl(&["bootstrap".into(), domain()?, path.to_string_lossy().into()])
}

/// Unload the job, stopping it if it is running.
pub fn bootout() -> Result<()> {
    launchctl(&["bootout".into(), target()?])
}

/// Start the job, or restart it when `force`. `kickstart -k` is the one call
/// that behaves the same whether or not the job is already running.
pub fn kickstart(force: bool) -> Result<()> {
    let mut args: Vec<String> = vec!["kickstart".into()];
    if force {
        args.push("-k".into());
    }
    args.push(target()?);
    launchctl(&args)
}

/// Ask the running job to stop, leaving it loaded so `start` can bring it back
/// without a re-bootstrap. `KeepAlive` is false, so it stays stopped.
pub fn stop() -> Result<()> {
    launchctl(&["kill".into(), "SIGTERM".into(), target()?])
}

fn launchctl(args: &[String]) -> Result<()> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .map_err(|e| Error::other(format!("could not run launchctl: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    let message = [&output.stderr[..], &output.stdout[..]]
        .into_iter()
        .map(|bytes| String::from_utf8_lossy(bytes).trim().to_owned())
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| format!("exit {}", output.status));
    Err(Error::other(format!(
        "launchctl {}: {message}",
        args.join(" ")
    )))
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logs() -> PathBuf {
        PathBuf::from("/Users/x/.subbier/logs")
    }

    #[test]
    fn the_plist_carries_the_keys_that_make_it_load_once_and_stay_quit() {
        let text = plist(
            Path::new("/Applications/Subbier.app/Contents/MacOS/Subbier"),
            &[],
            &logs(),
        );
        assert!(text.contains("<key>RunAtLoad</key>\n\t<true/>"), "{text}");
        assert!(text.contains("<key>KeepAlive</key>\n\t<false/>"), "{text}");
        assert!(text.contains("<string>Interactive</string>"), "{text}");
        assert!(text.contains("/Applications/Subbier.app/Contents/MacOS/Subbier"));
    }

    /// The generator and `installed_program`'s scanner must agree, escaping included.
    #[test]
    fn a_plist_round_trips_the_program_and_arguments_it_was_given() {
        let program = Path::new("/Users/a&b/<Sub bier>/subbier");
        let args = [
            "serve".to_owned(),
            "--config".to_owned(),
            "/tmp/c.kdl".to_owned(),
        ];
        let text = plist(program, &args, &logs());
        assert!(
            text.contains("/Users/a&amp;b/&lt;Sub bier&gt;/subbier"),
            "{text}"
        );
        assert!(!text.contains("/Users/a&b/"), "{text}");

        let array = text
            .split_once("<key>ProgramArguments</key>")
            .unwrap()
            .1
            .split_once("<array>")
            .unwrap()
            .1
            .split_once("</array>")
            .unwrap()
            .0;
        let read_back: Vec<String> = array
            .split("<string>")
            .skip(1)
            .filter_map(|s| s.split_once("</string>"))
            .map(|(v, _)| unescape(v))
            .collect();
        assert_eq!(
            read_back,
            [program.to_str().unwrap(), "serve", "--config", "/tmp/c.kdl"]
        );
    }

    #[test]
    fn the_label_and_the_basename_agree() {
        // launchctl matches the file to its Label; a mismatch loads nothing.
        let path = plist_path().expect("a home directory");
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            format!("{LABEL}.plist")
        );
        assert!(
            plist(Path::new("/x"), &[], &logs()).contains(&format!("<string>{LABEL}</string>"))
        );
    }

    #[test]
    fn the_launchd_target_names_the_gui_domain() {
        // `user/<uid>` cannot reach the login Keychain or the menu bar.
        let domain = domain().expect("a uid");
        assert!(domain.starts_with("gui/"), "{domain}");
        assert_eq!(target().unwrap(), format!("{domain}/{LABEL}"));
    }

    #[test]
    fn launchctl_list_output_is_parsed_for_pid_and_exit_status() {
        let sample = "{\n\t\"LimitLoadToSessionType\" = \"Aqua\";\n\t\"Label\" = \"com.github.anowell.subbier\";\n\t\"OnDemand\" = false;\n\t\"LastExitStatus\" = 1;\n\t\"PID\" = 4242;\n};\n";
        assert_eq!(plist_field(sample, "PID"), Some("4242"));
        assert_eq!(plist_field(sample, "LastExitStatus"), Some("1"));
        assert_eq!(plist_field(sample, "Nope"), None);
    }
}
