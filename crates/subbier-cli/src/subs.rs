//! `subbier subs` — which accounts subbier has, and where they came from.
//! Provenance is the point: most arrive by reading what `codex` and `claude`
//! are already logged in as, and which file or keychain an account came from is
//! what someone needs when it misbehaves.

use libsubby::snapshot::SubView;
use libsubby::{CredentialSource, Snapshot};

use crate::{GlobalArgs, Result, runtime};

pub async fn run(global: &GlobalArgs) -> Result {
    let (snap, local) = runtime::observe(global).await?;
    println!("{}", render_subs(&snap));
    if let Some(local) = local {
        local.shutdown().await;
    }
    Ok(())
}

fn render_subs(snap: &Snapshot) -> String {
    if snap.subs.is_empty() {
        return "No accounts. subbier adopts whatever codex and claude are logged into;\n\
                run `codex login`, `claude login`, or `subbier login codex`."
            .to_owned();
    }

    let mut rows = vec![[
        "PROVIDER".to_owned(),
        "LABEL".to_owned(),
        "PLAN".to_owned(),
        "SOURCE".to_owned(),
        "ENABLED".to_owned(),
    ]];
    rows.extend(snap.subs.iter().map(row));

    let widths: Vec<usize> = (0..5)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, cell)| {
                    if i + 1 == row.len() {
                        cell.clone()
                    } else {
                        format!("{cell:<width$}", width = widths[i])
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn row(sub: &SubView) -> [String; 5] {
    [
        sub.provider.id().to_owned(),
        sub.label.clone(),
        sub.plan.clone().unwrap_or_else(|| "-".to_owned()),
        source_label(&sub.source),
        if sub.enabled { "yes" } else { "no" }.to_owned(),
    ]
}

fn source_label(source: &CredentialSource) -> String {
    match source {
        CredentialSource::Adopted { from } => {
            format!("adopted {}", tildify(&from.to_string_lossy()))
        }
        CredentialSource::Keychain => "keychain".to_owned(),
        CredentialSource::Subbier => "subbier login".to_owned(),
    }
}

/// `/Users/me/.codex/auth.json` -> `~/.codex/auth.json`.
fn tildify(path: &str) -> String {
    let Some(home) = dirs::home_dir() else {
        return path.to_owned();
    };
    let home = home.to_string_lossy();
    match path.strip_prefix(home.as_ref()) {
        Some(rest) => format!("~{rest}"),
        None => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use libsubby::snapshot::{RoutingView, SnapshotData, SubHealth};
    use libsubby::{Provider, SubId};

    use super::*;

    fn sub(id: u32, provider: Provider, label: &str, source: CredentialSource) -> SubView {
        SubView {
            plan_tier: "unknown".into(),
            plan_weight: 1.0,
            id: SubId(id),
            key: libsubby::SubKey::new(provider, format!("acct-{id}")),
            provider,
            label: label.to_owned(),
            plan: Some("plus".to_owned()),
            source,
            enabled: true,
            health: SubHealth::Ok,
            session: None,
            weekly: None,
            scoped: Vec::new(),
            routing: RoutingView::default(),
        }
    }

    #[test]
    fn every_row_names_its_credential_source() {
        let snap = Snapshot::from(SnapshotData {
            generation: 2,
            subs: vec![
                sub(
                    1,
                    Provider::Codex,
                    "a@example.com",
                    CredentialSource::Adopted {
                        from: PathBuf::from("/nowhere/.codex/auth.json"),
                    },
                ),
                sub(
                    2,
                    Provider::Claude,
                    "b@example.com",
                    CredentialSource::Keychain,
                ),
                sub(
                    3,
                    Provider::Claude,
                    "c@example.com",
                    CredentialSource::Subbier,
                ),
            ],
            ..SnapshotData::default()
        });
        let text = render_subs(&snap);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("PROVIDER"), "{text}");
        assert!(
            lines[1].contains("adopted /nowhere/.codex/auth.json"),
            "{text}"
        );
        assert!(lines[2].contains("keychain"), "{text}");
        assert!(lines[3].contains("subbier login"), "{text}");
        let column = lines[0].find("LABEL").unwrap();
        for line in &lines[1..] {
            assert!(line[column..].starts_with(['a', 'b', 'c']), "{line}");
        }
    }

    #[test]
    fn no_subs_is_advice_not_an_empty_table() {
        let snap = Snapshot::from(SnapshotData {
            generation: 2,
            ..SnapshotData::default()
        });
        assert!(render_subs(&snap).contains("subbier login codex"));
    }
}
