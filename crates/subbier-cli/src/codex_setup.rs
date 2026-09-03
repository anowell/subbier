//! `subbier codex-setup` — the onboarding step `codex` actually obeys. Under
//! ChatGPT auth it ignores `OPENAI_BASE_URL` (verified on `codex-cli` 0.149.1);
//! only a custom model provider in `config.toml` redirects it. `model_provider`
//! is a **top-level** key, so appending the snippet silently buries it.

use std::path::{Path, PathBuf};

use libsubby::snapshot::ProxyView;

use crate::envcmd;
use crate::{GlobalArgs, Result, runtime};

/// Everything under this name is subbier's to rewrite; the rest is the user's.
const PROVIDER: &str = "subbier";

/// The wire protocol the proxy speaks on `/v1/responses`.
const WIRE_API: &str = "responses";

/// Point `codex` at the proxy. `codex` needs a config file, not env vars.
#[derive(Debug, Clone, clap::Args)]
pub struct CodexSetupArgs {
    /// Merge the provider block into `config.toml`, backing the file up first.
    #[arg(long)]
    pub write: bool,

    /// The `codex` config directory holding `config.toml`.
    #[arg(long, value_name = "PATH", env = "CODEX_HOME")]
    pub codex_home: Option<PathBuf>,
}

pub async fn run(global: &GlobalArgs, args: &CodexSetupArgs) -> Result {
    let (snap, local) = runtime::observe(global).await?;
    let outcome = act(&snap.proxy, args);
    if let Some(local) = local {
        local.shutdown().await;
    }
    outcome
}

/// No `await`, so the engine is never held open across a filesystem error.
fn act(proxy: &ProxyView, args: &CodexSetupArgs) -> Result {
    let (urls, notes) = envcmd::resolve(proxy);
    let path = config_path(args);
    let table = Table::new(&urls.openai, proxy.requires_key);

    if args.write {
        return write(&path, &table, &notes);
    }

    let mut out = String::new();
    for note in &notes {
        out.push_str(note);
        out.push('\n');
    }
    for line in table.snippet(&path) {
        out.push_str(&line);
        out.push('\n');
    }
    print!("{out}");

    // Context, not the snippet: `codex-setup > block.toml` must not catch it.
    match std::fs::read_to_string(&path) {
        Ok(existing) if table.plan(&existing).is_none() => {
            eprintln!(
                "subbier: note: {} already has this; codex is set up.",
                path.display()
            );
        }
        Ok(_) => eprintln!(
            "subbier: note: {} does not route codex through subbier yet. \
             Add the block above, or run `subbier codex-setup --write`.",
            path.display()
        ),
        Err(_) => eprintln!(
            "subbier: note: {} does not exist yet. \
             Create it with the block above, or run `subbier codex-setup --write`.",
            path.display()
        ),
    }
    if proxy.requires_key {
        eprintln!(
            "subbier: note: proxy.key is set, so the block reads the key from \
             $OPENAI_API_KEY — export it too (`subbier env --provider codex`)."
        );
    }
    Ok(())
}

fn write(path: &Path, table: &Table, notes: &[String]) -> Result {
    for note in notes {
        eprintln!("subbier: {}", note.trim_start_matches("# "));
    }

    let existing = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("cannot read {}: {e}", path.display()).into()),
    };

    let Some(edit) = table.plan(existing.as_deref().unwrap_or_default()) else {
        println!(
            "{} already routes codex through subbier; nothing to do.",
            path.display()
        );
        return Ok(());
    };

    // A failed backup must abort, not proceed unprotected.
    if existing.is_some() {
        let backup = backup_path(path);
        std::fs::copy(path, &backup).map_err(|e| {
            format!(
                "cannot back up {} to {}: {e}",
                path.display(),
                backup.display()
            )
        })?;
        println!("backed up {} -> {}", path.display(), backup.display());
    } else if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }

    std::fs::write(path, &edit.text)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;

    println!(
        "{} {}",
        if existing.is_some() {
            "updated"
        } else {
            "created"
        },
        path.display()
    );
    for change in &edit.changes {
        println!("  {change}");
    }
    println!(
        "codex now routes through subbier. Check it with `codex exec 'say hi'`, then `subbier status`."
    );
    Ok(())
}

/// An empty `CODEX_HOME` means unset, as [`libsubby::auth::discovery`] has it.
fn config_path(args: &CodexSetupArgs) -> PathBuf {
    let home = match args.codex_home.as_deref() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".codex"),
    };
    home.join("config.toml")
}

/// Unused, so a second run in the same second cannot overwrite the original.
fn backup_path(path: &Path) -> PathBuf {
    let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ");
    let base = path.as_os_str().to_string_lossy().into_owned();
    let first = PathBuf::from(format!("{base}.subbier-{stamp}.bak"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| PathBuf::from(format!("{base}.subbier-{stamp}-{n}.bak")))
        .find(|p| !p.exists())
        .expect("an unused backup name exists")
}

/// The `[model_providers.subbier]` table subbier owns, as ordered key/values.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Table {
    keys: Vec<(&'static str, String)>,
}

impl Table {
    /// `env_key` appears **only** when `proxy.key` is set: named with the
    /// variable unset, `codex` refuses to start at all.
    fn new(base_url: &str, requires_key: bool) -> Self {
        let mut keys = vec![
            ("name", PROVIDER.to_owned()),
            ("base_url", base_url.to_owned()),
            ("wire_api", WIRE_API.to_owned()),
        ];
        if requires_key {
            keys.push(("env_key", "OPENAI_API_KEY".to_owned()));
        }
        Self { keys }
    }

    fn header() -> String {
        format!("[model_providers.{PROVIDER}]")
    }

    fn lines(&self) -> Vec<String> {
        std::iter::once(Self::header())
            .chain(self.keys.iter().map(|(k, v)| assignment(k, v)))
            .collect()
    }

    fn snippet(&self, path: &Path) -> Vec<String> {
        let mut lines = vec![
            format!(
                "# Add to {} — codex signed in with ChatGPT ignores OPENAI_BASE_URL.",
                path.display()
            ),
            "# `model_provider` is a TOP-LEVEL key: it must go above the first".to_owned(),
            "# [table] header, so do not append this file blindly.".to_owned(),
            assignment("model_provider", PROVIDER),
            String::new(),
        ];
        lines.extend(self.lines());
        lines
    }

    fn plan(&self, existing: &str) -> Option<Edit> {
        let mut lines: Vec<String> = existing.lines().map(str::to_owned).collect();
        let mut changes = Vec::new();

        self.merge_table(&mut lines, &mut changes);
        // After the merge: appending the table can create the first `[` header,
        // which is what bounds the top-level region.
        self.set_model_provider(&mut lines, &mut changes);

        if changes.is_empty() {
            return None;
        }
        let mut text = lines.join("\n");
        text.push('\n');
        Some(Edit { text, changes })
    }

    /// Key-level, never wholesale, so a hand-added key in our table survives.
    fn merge_table(&self, lines: &mut Vec<String>, changes: &mut Vec<String>) {
        let Some(range) = table_range(lines, &Self::header()) else {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.extend(self.lines());
            changes.push(format!("added {}", Self::header()));
            return;
        };

        // Index 0 of the body is the header, which assigns nothing.
        let mut body: Vec<String> = lines[range.clone()].to_vec();
        for (key, value) in &self.keys {
            let want = assignment(key, value);
            match body.iter().position(|l| assigns(l, key)) {
                Some(i) if body[i] == want => {}
                Some(i) => {
                    changes.push(format!("{} {} -> {want}", Self::header(), body[i].trim()));
                    body[i] = want;
                }
                None => {
                    changes.push(format!("{} += {want}", Self::header()));
                    body.push(want);
                }
            }
        }
        lines.splice(range, body);
    }

    fn set_model_provider(&self, lines: &mut Vec<String>, changes: &mut Vec<String>) {
        let want = assignment("model_provider", PROVIDER);
        let top_end = lines
            .iter()
            .position(|l| l.trim_start().starts_with('['))
            .unwrap_or(lines.len());

        match lines[..top_end]
            .iter()
            .position(|l| assigns(l, "model_provider"))
        {
            Some(i) if lines[i] == want => {}
            Some(i) => {
                // Commented rather than deleted, as a signpost.
                let old = lines[i].clone();
                changes.push(format!("model_provider: {} -> {want}", old.trim()));
                lines[i] = want;
                lines.insert(
                    i,
                    format!(
                        "# {old}  # replaced by `subbier codex-setup`; restore by uncommenting"
                    ),
                );
            }
            None => {
                // A blank line on each side, unless there already is one.
                let mut insert = vec![want.clone()];
                if top_end > 0 && !lines[top_end - 1].trim().is_empty() {
                    insert.insert(0, String::new());
                }
                if top_end < lines.len() && !lines[top_end].trim().is_empty() {
                    insert.push(String::new());
                }
                lines.splice(top_end..top_end, insert);
                changes.push(format!("added {want}"));
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Edit {
    text: String,
    changes: Vec<String>,
}

/// `key = "value"`, TOML's basic string.
fn assignment(key: &str, value: &str) -> String {
    format!(
        "{key} = \"{}\"",
        value.replace('\\', r"\\").replace('"', "\\\"")
    )
}

/// Does this line assign exactly `key`? `model_provider` must not match
/// `model_providers`, and a commented-out assignment must not match at all.
fn assigns(line: &str, key: &str) -> bool {
    line.trim_start()
        .strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// Header included, trailing blank lines excluded.
fn table_range(lines: &[String], header: &str) -> Option<std::ops::Range<usize>> {
    let start = lines.iter().position(|l| l.trim() == header)?;
    let mut end = lines[start + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map_or(lines.len(), |i| start + 1 + i);
    while end > start + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    Some(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Table {
        Table::new("http://127.0.0.1:8787/v1", false)
    }

    fn args() -> CodexSetupArgs {
        CodexSetupArgs {
            write: false,
            codex_home: None,
        }
    }

    /// Verified end to end against a live proxy; if this changes, re-verify it.
    #[test]
    fn the_block_is_exactly_the_verified_four_lines() {
        assert_eq!(
            table().lines(),
            vec![
                "[model_providers.subbier]",
                "name = \"subbier\"",
                "base_url = \"http://127.0.0.1:8787/v1\"",
                "wire_api = \"responses\"",
            ]
        );
    }

    #[test]
    fn an_empty_file_becomes_a_complete_working_config() {
        let edit = table().plan("").expect("an empty file needs everything");
        assert_eq!(
            edit.text,
            "model_provider = \"subbier\"\n\
             \n\
             [model_providers.subbier]\n\
             name = \"subbier\"\n\
             base_url = \"http://127.0.0.1:8787/v1\"\n\
             wire_api = \"responses\"\n"
        );
    }

    /// Appending it below a table header would bury it and silently do nothing.
    #[test]
    fn model_provider_lands_above_the_first_table_header() {
        let existing = "model = \"gpt-5.5\"\n\n[projects.\"/tmp/x\"]\ntrust_level = \"trusted\"\n";
        let edit = table().plan(existing).expect("not configured yet");
        let key = edit
            .text
            .lines()
            .position(|l| l == "model_provider = \"subbier\"");
        let first_table = edit.text.lines().position(|l| l.starts_with('['));
        assert!(key < first_table, "{}", edit.text);
        assert!(edit.text.contains("model = \"gpt-5.5\""));
        assert!(edit.text.contains("[projects.\"/tmp/x\"]"));
        assert!(edit.text.contains("trust_level = \"trusted\""));
    }

    #[test]
    fn running_it_twice_changes_nothing_the_second_time() {
        let once = table().plan("").expect("first run writes").text;
        assert_eq!(table().plan(&once), None);

        let existing =
            "# my notes\nmodel = \"gpt-5.5\"\n\n[projects.\"/tmp/x\"]\ntrust_level = \"trusted\"\n";
        let first = table().plan(existing).expect("first run writes").text;
        assert_eq!(table().plan(&first), None, "{first}");
    }

    #[test]
    fn comments_and_unrelated_tables_survive_verbatim() {
        let existing = "# keep me\nmodel = \"gpt-5.5\"  # and me\n\n[tui]\nnotifications = true\n";
        let edit = table().plan(existing).expect("not configured yet");
        assert!(edit.text.contains("# keep me\n"), "{}", edit.text);
        assert!(
            edit.text.contains("model = \"gpt-5.5\"  # and me\n"),
            "{}",
            edit.text
        );
        assert!(
            edit.text.contains("[tui]\nnotifications = true\n"),
            "{}",
            edit.text
        );
    }

    #[test]
    fn a_foreign_model_provider_is_commented_out_rather_than_deleted() {
        let edit = table()
            .plan("model_provider = \"myproxy\"\n")
            .expect("it points somewhere else");
        assert!(
            edit.text
                .contains("# model_provider = \"myproxy\"  # replaced by `subbier codex-setup`"),
            "{}",
            edit.text
        );
        assert!(
            edit.text.contains("\nmodel_provider = \"subbier\"\n"),
            "{}",
            edit.text
        );
        assert!(
            edit.changes.iter().any(|c| c.contains("myproxy")),
            "{:?}",
            edit.changes
        );
    }

    #[test]
    fn extra_keys_in_our_own_table_are_kept_and_a_stale_port_is_corrected() {
        let existing = "model_provider = \"subbier\"\n\n\
                        [model_providers.subbier]\n\
                        name = \"subbier\"\n\
                        base_url = \"http://127.0.0.1:9999/v1\"\n\
                        wire_api = \"responses\"\n\
                        request_max_retries = 5\n";
        let edit = table().plan(existing).expect("the port moved");
        assert!(
            edit.text
                .contains("base_url = \"http://127.0.0.1:8787/v1\""),
            "{}",
            edit.text
        );
        assert!(!edit.text.contains("9999"), "{}", edit.text);
        assert!(
            edit.text.contains("request_max_retries = 5"),
            "{}",
            edit.text
        );
        assert_eq!(edit.changes.len(), 1, "{:?}", edit.changes);
    }

    #[test]
    fn the_merge_stops_at_the_next_table_header() {
        let existing = "model_provider = \"subbier\"\n\n\
                        [model_providers.subbier]\n\
                        name = \"subbier\"\n\n\
                        [model_providers.other]\n\
                        name = \"other\"\n\
                        base_url = \"https://example.test/v1\"\n";
        let edit = table().plan(existing).expect("keys are missing");
        assert!(
            edit.text.contains("[model_providers.other]\nname = \"other\"\nbase_url = \"https://example.test/v1\"\n"),
            "{}",
            edit.text
        );
        // Ours gained the missing keys, before the other table starts.
        let ours = edit.text.find("[model_providers.subbier]").unwrap();
        let other = edit.text.find("[model_providers.other]").unwrap();
        let mine = &edit.text[ours..other];
        assert!(
            mine.contains("base_url = \"http://127.0.0.1:8787/v1\""),
            "{mine}"
        );
        assert!(mine.contains("wire_api = \"responses\""), "{mine}");
    }

    /// `model_provider` and `model_providers` differ by one character.
    #[test]
    fn key_matching_is_not_fooled_by_prefixes_or_comments() {
        assert!(assigns("model_provider = \"x\"", "model_provider"));
        assert!(assigns("  model_provider   =   \"x\"", "model_provider"));
        assert!(!assigns("model_providers = { }", "model_provider"));
        assert!(!assigns("# model_provider = \"x\"", "model_provider"));
        assert!(!assigns("[model_providers.subbier]", "model_provider"));
    }

    #[test]
    fn a_model_provider_inside_a_table_is_not_the_top_level_one() {
        let existing = "[profiles.work]\nmodel_provider = \"myproxy\"\n";
        let edit = table().plan(existing).expect("no top-level key yet");
        assert!(
            edit.text.starts_with("model_provider = \"subbier\"\n"),
            "{}",
            edit.text
        );
        assert!(
            edit.text
                .contains("[profiles.work]\nmodel_provider = \"myproxy\"\n"),
            "{}",
            edit.text
        );
    }

    #[test]
    fn a_keyed_proxy_names_the_env_var_codex_should_read_the_key_from() {
        let keyed = Table::new("http://127.0.0.1:8787/v1", true);
        assert!(
            keyed
                .lines()
                .contains(&"env_key = \"OPENAI_API_KEY\"".to_owned())
        );
        // `codex` refuses to start when `env_key` names an unset variable.
        assert!(!table().lines().iter().any(|l| l.starts_with("env_key")));
    }

    #[test]
    fn the_snippet_warns_that_the_key_is_top_level() {
        let lines = table().snippet(Path::new("/home/me/.codex/config.toml"));
        assert!(
            lines[0].contains("/home/me/.codex/config.toml"),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("TOP-LEVEL")), "{lines:?}");
        assert!(lines.contains(&"model_provider = \"subbier\"".to_owned()));
        assert!(lines.contains(&Table::header()));
    }

    #[test]
    fn codex_home_is_honoured_and_an_empty_value_means_unset() {
        let explicit = CodexSetupArgs {
            codex_home: Some(PathBuf::from("/tmp/scratch-codex")),
            ..args()
        };
        assert_eq!(
            config_path(&explicit),
            PathBuf::from("/tmp/scratch-codex/config.toml")
        );

        let empty = CodexSetupArgs {
            codex_home: Some(PathBuf::new()),
            ..args()
        };
        assert!(config_path(&empty).ends_with(".codex/config.toml"));
        assert!(config_path(&args()).ends_with(".codex/config.toml"));
    }
}
