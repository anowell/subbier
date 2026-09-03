//! `subbier service` — install and drive the background subbier. The mechanism
//! is the same `~/Library/LaunchAgents` plist as "launch at login", so the two
//! must be kept in step. **The service is the menu bar app** — same engine and
//! proxy, plus the status item; `--headless` installs `subbier serve` instead.

use std::path::PathBuf;
use std::time::Duration;

/// Manage the background subbier: the launchd agent, and the process it runs.
#[derive(Debug, Clone, clap::Args)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Subcommand)]
pub enum Action {
    /// Write the launch agent, load it, and start subbier.
    Install(InstallArgs),
    /// Start the service.
    Start,
    /// Stop the service, leaving it installed.
    Stop,
    /// Stop and start the service.
    Restart,
    /// What is installed, what launchd knows, and what is on the port.
    Status,
    /// Stop the service and remove the launch agent.
    Uninstall,
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct InstallArgs {
    /// Run `subbier serve` rather than the menu bar app.
    #[arg(long)]
    pub headless: bool,

    /// The program to run, instead of the one that would be chosen. Must be an
    /// absolute path: launchd searches no `PATH`.
    #[arg(long, value_name = "PATH")]
    pub program: Option<PathBuf>,
}

const START_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(250);

#[cfg(not(target_os = "macos"))]
pub async fn run(_global: &crate::GlobalArgs, _args: &ServiceArgs) -> crate::Result {
    Err(
        "`subbier service` is macOS-only (it manages a launchd agent). \
         Run `subbier serve` under your own supervisor — systemd, runit, or a terminal."
            .into(),
    )
}

#[cfg(target_os = "macos")]
pub use macos::run;

#[cfg(target_os = "macos")]
mod macos {
    use super::{Action, InstallArgs, POLL, START_TIMEOUT, STOP_TIMEOUT, ServiceArgs};
    use crate::{GlobalArgs, Result, runtime};

    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use libsubby::instance::probe;
    use libsubby::service::{self, JobState};
    use libsubby::{Config, Snapshot};

    pub async fn run(global: &GlobalArgs, args: &ServiceArgs) -> Result {
        let config = runtime::load_config(global)?;
        match &args.action {
            Action::Install(install) => self::install(global, &config, install).await,
            Action::Start => start(&config, false).await,
            Action::Stop => stop(&config).await,
            Action::Restart => start(&config, true).await,
            Action::Status => status(&config).await,
            Action::Uninstall => uninstall(global, &config).await,
        }
    }

    /// The menu bar app is the default; `subbier serve` is the CLI-only fallback.
    fn program_for(global: &GlobalArgs, args: &InstallArgs) -> Result<(PathBuf, Vec<String>)> {
        let me = std::env::current_exe()
            .map_err(|e| format!("could not find this binary's own path: {e}"))?;

        if let Some(program) = &args.program {
            if !program.is_absolute() {
                return Err(format!(
                    "--program needs an absolute path; launchd searches no PATH ({} is relative)",
                    program.display()
                )
                .into());
            }
            return Ok((program.clone(), serve_args(global, program)));
        }

        if !args.headless {
            match menubar_beside(&me) {
                Some(menubar) => {
                    // It reads `$SUBBIER_HOME/config.kdl` and takes no flags.
                    if global.config.is_some() {
                        return Err(format!(
                            "the menu bar app has no --config flag, so it would ignore {}.\n\
                             Install the headless proxy instead (`subbier service install --headless`), \
                             or point SUBBIER_HOME at that config's directory.",
                            runtime::config_path(global).display()
                        )
                        .into());
                    }
                    return Ok((menubar, Vec::new()));
                }
                // Silence here reads as "the service did not start the menu bar".
                None => println!(
                    "no `subbier-menubar` next to {}, so this installs the headless proxy.\n  \
                     For the menu bar app: cargo install --git https://github.com/anowell/subbier subbier-macos",
                    me.display()
                ),
            }
        }

        Ok((me.clone(), serve_args(global, &me)))
    }

    /// Beside the invoked path *and* the real one: `current_exe` on macOS hands
    /// back the symlink it was called through.
    fn menubar_beside(me: &Path) -> Option<PathBuf> {
        let mut roots = vec![me.to_path_buf()];
        match std::fs::canonicalize(me) {
            Ok(real) if real != me => roots.push(real),
            _ => {}
        }
        roots
            .iter()
            .map(|root| root.with_file_name("subbier-menubar"))
            .find(|candidate| candidate.is_file())
    }

    /// The installed service must read the same config the install command did.
    fn serve_args(global: &GlobalArgs, program: &Path) -> Vec<String> {
        if program.file_name().is_some_and(|n| n == "subbier-menubar") {
            return Vec::new();
        }
        let mut args = vec!["serve".to_owned()];
        if global.config.is_some() {
            args.push("--config".to_owned());
            args.push(runtime::config_path(global).to_string_lossy().into_owned());
        }
        args
    }

    /// The menu bar app reconciles this setting against the filesystem on every
    /// publish: leave them disagreeing and it rewrites or deletes the plist.
    fn set_launch_at_login(global: &GlobalArgs, on: bool) -> Result<()> {
        let path = runtime::config_path(global);
        libsubby::config::write::set_launch_at_login(&path, on)?;
        Ok(())
    }

    async fn install(global: &GlobalArgs, config: &Config, args: &InstallArgs) -> Result {
        let (program, program_args) = program_for(global, args)?;
        if !program.is_file() {
            return Err(format!("{} is not a file", program.display()).into());
        }

        // launchd will not bootstrap over a loaded job, so a reinstall stops it.
        if service::job_state().loaded {
            println!("stopping the running service to replace it");
            service::bootout()?;
            let _ = wait_until_down(config).await;
        }

        service::install(&program, &program_args)?;
        let path = service::plist_path().ok_or("no home directory")?;
        println!("wrote {}", path.display());
        println!("  runs {}", command_line(&program, &program_args));
        set_launch_at_login(global, true)?;

        // `RunAtLoad` is true, so bootstrapping also starts it.
        service::bootstrap()?;
        report_start(config).await
    }

    async fn start(config: &Config, restart: bool) -> Result {
        if !service::is_installed() {
            return Err("no launch agent installed. Run `subbier service install` first.".into());
        }
        let state = service::job_state();
        if state.loaded {
            service::kickstart(restart)?;
        } else {
            // With `RunAtLoad` set, bootstrapping is the start.
            service::bootstrap()?;
        }
        report_start(config).await
    }

    async fn stop(config: &Config) -> Result {
        let state = service::job_state();
        if !state.loaded {
            println!("the service is not loaded; nothing to stop");
            return Ok(());
        }
        if state.pid.is_none() {
            println!("the service is loaded but not running; nothing to stop");
            return Ok(());
        }
        service::stop()?;
        if wait_until_down(config).await {
            println!("stopped");
        } else {
            // The signal landed but something still holds the port.
            println!(
                "sent SIGTERM, but something still answers on {}. `subbier service status` says what.",
                config.proxy.bind
            );
        }
        Ok(())
    }

    async fn uninstall(global: &GlobalArgs, config: &Config) -> Result {
        // Before removing the file, so a menu bar app that publishes in the gap
        // does not helpfully write it back.
        set_launch_at_login(global, false)?;
        if service::job_state().loaded {
            service::bootout()?;
            let _ = wait_until_down(config).await;
            println!("stopped and unloaded the service");
        }
        let path = service::plist_path().ok_or("no home directory")?;
        let existed = path.exists();
        service::remove()?;
        if existed {
            println!("removed {}", path.display());
        } else {
            println!("no launch agent was installed");
        }
        Ok(())
    }

    async fn status(config: &Config) -> Result {
        let job = service::job_state();
        let snap = probe(config).await;
        println!(
            "{}",
            render_status(
                service::plist_path().as_deref(),
                service::installed_program(),
                job,
                &config.proxy.bind,
                snap.as_ref(),
            )
        );
        Ok(())
    }

    enum Started {
        Up(Box<Snapshot>),
        /// A subbier that is not the job holds the port, whatever launchctl said.
        PortTaken(Option<u32>),
    }

    /// launchctl succeeding does not mean subbier is up: launchd reports that it
    /// ran the program, then the program finds the port held and exits.
    async fn report_start(config: &Config) -> Result {
        match settle(config).await {
            Some(Started::Up(snap)) => {
                let base = snap
                    .proxy
                    .anthropic_base_url
                    .clone()
                    .unwrap_or_else(|| config.proxy.bind.clone());
                let pid = snap
                    .proxy
                    .pid
                    .map_or_else(String::new, |pid| format!(" (pid {pid})"));
                println!("subbier is running{pid}, serving {base}");
                Ok(())
            }
            Some(Started::PortTaken(pid)) => Err(port_taken(pid, &config.proxy.bind).into()),
            None => {
                let job = service::job_state();
                Err(started_but_silent(job, &config.proxy.bind).into())
            }
        }
    }

    /// Poll until the job and the port agree, or one rules the other out.
    async fn settle(config: &Config) -> Option<Started> {
        let settled = wait(START_TIMEOUT, || async {
            let job = service::job_state();
            let snap = probe(config).await;
            match (job.pid, snap) {
                // The job is running and it is what answers: the only success.
                (Some(job_pid), Some(snap)) if snap.proxy.pid == Some(job_pid) => {
                    Some(Started::Up(Box::new(snap)))
                }
                // Two known, differing pids will never reconcile.
                (Some(_), Some(snap)) if snap.proxy.pid.is_some() => {
                    Some(Started::PortTaken(snap.proxy.pid))
                }
                _ => None,
            }
        })
        .await;
        if settled.is_some() {
            return settled;
        }

        // Out of budget: a subbier on the port with no job behind it means the
        // job ran, found the port held, and exited.
        let job = service::job_state();
        match (job.pid, probe(config).await) {
            (None, Some(snap)) => Some(Started::PortTaken(snap.proxy.pid)),
            _ => None,
        }
    }

    fn port_taken(pid: Option<u32>, bind: &str) -> String {
        let who = pid.map_or_else(
            || "another subbier".to_owned(),
            |pid| format!("another subbier (pid {pid})"),
        );
        format!(
            "the launch agent ran, but {who} already owns {bind}, so the service exited.\n\
             Stop that process, or give the service its own `proxy.bind` in config.kdl."
        )
    }

    /// Nothing identifiable answered: point at the reason, not the symptom.
    fn started_but_silent(job: JobState, bind: &str) -> String {
        let mut lines = vec![format!(
            "started the launch agent, but nothing answered on {bind}."
        )];
        match (job.pid, job.last_exit_status) {
            (Some(pid), _) => lines.push(format!(
                "The process is alive (pid {pid}) but is not serving; \
                 `subbier service status` says more."
            )),
            (None, Some(code)) if code != 0 => lines.push(format!(
                "It exited with status {code} — most often the port is already \
                 taken by another subbier."
            )),
            (None, _) => {
                lines.push("The process is not running. It may have exited immediately.".to_owned())
            }
        }
        lines.push(format!(
            "See {}.",
            libsubby::store::home()
                .join("logs/launchd.err.log")
                .display()
        ));
        lines.join("\n")
    }

    fn render_status(
        plist: Option<&Path>,
        installed: Option<Vec<String>>,
        job: JobState,
        bind: &str,
        snap: Option<&Snapshot>,
    ) -> String {
        let mut out = Vec::new();

        out.push("LAUNCH AGENT".to_owned());
        match (plist, plist.is_some_and(Path::exists)) {
            (Some(path), true) => {
                out.push(format!("  installed  {}", path.display()));
                if let Some(args) = installed {
                    out.push(format!("  runs       {}", args.join(" ")));
                }
            }
            (Some(path), false) => {
                out.push(format!(
                    "  not installed  ({} does not exist)",
                    path.display()
                ));
            }
            (None, _) => out.push("  not installed  (no home directory)".to_owned()),
        }

        out.push(String::new());
        out.push("LAUNCHD".to_owned());
        if job.loaded {
            match job.pid {
                Some(pid) => out.push(format!("  running    pid {pid}")),
                None => out.push("  loaded, not running".to_owned()),
            }
            if let Some(code) = job.last_exit_status
                && code != 0
            {
                out.push(format!("  last exit  {code}"));
            }
        } else {
            out.push("  not loaded".to_owned());
        }

        out.push(String::new());
        out.push(format!("PORT  {bind}"));
        match snap {
            None => out.push("  nothing is answering".to_owned()),
            Some(snap) => {
                let version = snap.proxy.version.as_deref().unwrap_or("unknown version");
                match snap.proxy.pid {
                    Some(pid) => {
                        out.push(format!("  a subbier is answering  pid {pid} · {version}"));
                        // Without a pid, "the service is up" and "something is
                        // on its port" look identical.
                        match job.pid {
                            Some(job_pid) if job_pid == pid => {
                                out.push("  this is the launchd service".to_owned());
                            }
                            Some(_) | None => out.push(
                                "  this is NOT the launchd service — another subbier holds the port"
                                    .to_owned(),
                            ),
                        }
                    }
                    None => out.push(format!(
                        "  a subbier is answering  ({version}, too old to report its pid)"
                    )),
                }
                out.push(format!("  {} account(s)", snap.subs.len()));
            }
        }

        out.join("\n")
    }

    fn command_line(program: &Path, args: &[String]) -> String {
        std::iter::once(program.to_string_lossy().into_owned())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    async fn wait_until_down(config: &Config) -> bool {
        wait(STOP_TIMEOUT, || async {
            probe(config).await.is_none().then_some(())
        })
        .await
        .is_some()
    }

    async fn wait<T, F, Fut>(budget: Duration, mut check: F) -> Option<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Option<T>>,
    {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(value) = check().await {
                return Some(value);
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use libsubby::snapshot::{ProxyView, SnapshotData};

        fn snapshot(pid: Option<u32>) -> Snapshot {
            Snapshot::from(SnapshotData {
                generation: 2,
                proxy: ProxyView {
                    running: true,
                    listening: Some("127.0.0.1:8787".parse().unwrap()),
                    pid,
                    version: Some("0.1.0".to_owned()),
                    ..ProxyView::default()
                },
                ..SnapshotData::default()
            })
        }

        #[test]
        fn status_says_whether_the_port_is_held_by_the_service_or_by_something_else() {
            let mine = render_status(
                Some(Path::new("/tmp/x.plist")),
                None,
                JobState {
                    loaded: true,
                    pid: Some(222),
                    last_exit_status: None,
                },
                "127.0.0.1:8787",
                Some(&snapshot(Some(222))),
            );
            assert!(mine.contains("this is the launchd service"), "{mine}");
            assert!(!mine.contains("NOT the launchd"), "{mine}");

            let theirs = render_status(
                Some(Path::new("/tmp/x.plist")),
                None,
                JobState {
                    loaded: true,
                    pid: Some(111),
                    last_exit_status: Some(0),
                },
                "127.0.0.1:8787",
                Some(&snapshot(Some(222))),
            );
            assert!(theirs.contains("NOT the launchd service"), "{theirs}");
        }

        #[test]
        fn status_on_a_clean_machine_reports_every_layer_as_absent() {
            let text = render_status(
                Some(Path::new("/nope/x.plist")),
                None,
                JobState::default(),
                "127.0.0.1:8787",
                None,
            );
            assert!(text.contains("not installed"), "{text}");
            assert!(text.contains("not loaded"), "{text}");
            assert!(text.contains("nothing is answering"), "{text}");
        }

        /// launchctl reports success, something answers on the port, and it is not the service.
        #[test]
        fn a_start_that_lost_the_port_names_who_holds_it() {
            let text = port_taken(Some(1234), "127.0.0.1:8787");
            assert!(text.contains("another subbier (pid 1234)"), "{text}");
            assert!(text.contains("127.0.0.1:8787"), "{text}");
            assert!(text.contains("proxy.bind"), "{text}");

            // An older instance reports no pid, and the sentence still reads.
            let text = port_taken(None, "127.0.0.1:8787");
            assert!(text.contains("another subbier already owns"), "{text}");
        }

        /// A nonzero exit right after a start is the port-conflict case.
        #[test]
        fn a_silent_start_points_at_the_exit_status_and_the_log() {
            let job = JobState {
                loaded: true,
                pid: None,
                last_exit_status: Some(1),
            };
            let text = started_but_silent(job, "127.0.0.1:8787");
            assert!(text.contains("exited with status 1"), "{text}");
            assert!(text.contains("port is already"), "{text}");
            assert!(text.contains("launchd.err.log"), "{text}");
        }

        #[test]
        fn the_installed_command_line_matches_the_program_it_names() {
            let with_config = GlobalArgs {
                config: Some(PathBuf::from("/tmp/c.kdl")),
                verbose: 0,
            };
            // The menu bar app has no --config flag; passing it `serve` crashes it.
            assert!(
                serve_args(&with_config, Path::new("/usr/local/bin/subbier-menubar")).is_empty()
            );
            assert_eq!(
                serve_args(&with_config, Path::new("/usr/local/bin/subbier")),
                ["serve", "--config", "/tmp/c.kdl"]
            );
            assert_eq!(
                serve_args(
                    &GlobalArgs {
                        config: None,
                        verbose: 0
                    },
                    Path::new("/usr/local/bin/subbier")
                ),
                ["serve"]
            );
        }

        /// `~/bin/subbier` symlinked into a build directory, with the menu bar app beside the real binary.
        #[test]
        fn the_menu_bar_app_is_found_through_a_symlinked_subbier() {
            let dir = std::env::temp_dir().join(format!("subbier-svc-{}", std::process::id()));
            let real = dir.join("real");
            let link_dir = dir.join("link");
            std::fs::create_dir_all(&real).unwrap();
            std::fs::create_dir_all(&link_dir).unwrap();
            std::fs::write(real.join("subbier"), b"x").unwrap();
            std::fs::write(real.join("subbier-menubar"), b"x").unwrap();

            let link = link_dir.join("subbier");
            std::os::unix::fs::symlink(real.join("subbier"), &link).unwrap();

            assert!(!link_dir.join("subbier-menubar").exists());
            // Compared canonically: on macOS `/var` is itself a symlink to
            // `/private/var`, so the temp paths differ by prefix alone.
            assert_eq!(
                menubar_beside(&link).map(|p| std::fs::canonicalize(p).unwrap()),
                Some(std::fs::canonicalize(real.join("subbier-menubar")).unwrap()),
                "the sibling has to be found through the symlink"
            );

            let alone = dir.join("alone");
            std::fs::create_dir_all(&alone).unwrap();
            std::fs::write(alone.join("subbier"), b"x").unwrap();
            assert_eq!(menubar_beside(&alone.join("subbier")), None);

            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
