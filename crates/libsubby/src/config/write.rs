//! Comment-preserving write-back for `config.kdl`: the menu writes on every
//! toggle, so this never round-trips through the typed structs in [`super`]. It
//! mutates the one node being set in a parsed [`KdlDocument`] — reusing its
//! existing entry, so inline formatting survives — and writes back atomically.

use std::path::Path;

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};

use crate::error::{Error, Result};

/// Set one dotted key in `path`, creating intermediate nodes — and the file
/// itself — as needed.
///
/// Recognised keys are `"<block>.<key>"` and `"sub.<sub-key>.<key>"`. A sub key
/// may itself contain dots (an email-derived one does), so everything between
/// `sub.` and the **last** dot is taken as the sub key.
pub fn set(path: &Path, key: &str, value: KdlValue) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };

    let mut doc: KdlDocument = existing.as_deref().unwrap_or("").parse()?;
    set_in(&mut doc, key, value)?;

    let mut out = doc.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    crate::store::write_atomic(path, out.as_bytes(), mode_for(path))
}

/// Set `ui.launch-at-login`. Named on its own because the menu bar app
/// reconciles this key against the launch-agent plist on every publish, so
/// anything installing or removing that agent must move the key with it.
pub fn set_launch_at_login(path: &Path, on: bool) -> Result<()> {
    set(path, "ui.launch-at-login", KdlValue::Bool(on))
}

/// [`set`], against an already-parsed document, to batch several changes.
pub fn set_in(doc: &mut KdlDocument, key: &str, value: KdlValue) -> Result<()> {
    let (steps, leaf) = split_key(key)?;

    let mut current = doc;
    for (depth, step) in steps.iter().enumerate() {
        let index = match find(current, step) {
            Some(index) => index,
            None => {
                let node = new_node(current, &step.source(), depth);
                current.nodes_mut().push(node);
                current.nodes().len() - 1
            }
        };
        let node = &mut current.nodes_mut()[index];
        open_children(node);
        current = node.ensure_children();
    }

    match find(current, &Step::Named(leaf)) {
        Some(index) => set_first_arg(&mut current.nodes_mut()[index], value),
        None => {
            let source = format!("{leaf} {}", repr(&value));
            let node = new_node(current, &source, steps.len());
            current.nodes_mut().push(node);
        }
    }
    Ok(())
}

/// One step down the document tree.
enum Step<'a> {
    Named(&'a str),
    /// A `sub` node addressed by its first argument, the [`crate::SubKey`].
    Sub(&'a str),
}

impl Step<'_> {
    /// The KDL text that creates this node, sans indentation.
    fn source(&self) -> String {
        match self {
            Step::Named(name) => (*name).to_string(),
            Step::Sub(key) => format!("sub {}", repr(&KdlValue::String((*key).to_string()))),
        }
    }
}

/// Split `"a.b"` into the nodes to walk and the leaf to set.
fn split_key(key: &str) -> Result<(Vec<Step<'_>>, &str)> {
    let bad = || Error::config(format!("{key:?} is not a settable config key"));

    if let Some(rest) = key.strip_prefix("sub.") {
        let (sub_key, leaf) = rest.rsplit_once('.').ok_or_else(bad)?;
        if sub_key.is_empty() || leaf.is_empty() {
            return Err(bad());
        }
        return Ok((vec![Step::Sub(sub_key)], leaf));
    }

    let mut parts: Vec<&str> = key.split('.').collect();
    let leaf = parts.pop().ok_or_else(bad)?;
    if leaf.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return Err(bad());
    }
    Ok((parts.into_iter().map(Step::Named).collect(), leaf))
}

/// The index of the node this step addresses, if the document has one.
fn find(doc: &KdlDocument, step: &Step<'_>) -> Option<usize> {
    doc.nodes().iter().position(|node| match step {
        Step::Named(name) => node.name().value() == *name,
        Step::Sub(key) => {
            node.name().value() == "sub"
                && node
                    .entries()
                    .iter()
                    .find(|e| e.name().is_none())
                    .and_then(|e| e.value().as_string())
                    == Some(*key)
        }
    })
}

/// Replace a node's first positional argument, keeping the entry's own
/// leading/trailing text so an inline `// comment` after the value survives.
fn set_first_arg(node: &mut KdlNode, value: KdlValue) {
    let text = repr(&value);
    match node.entries_mut().iter_mut().find(|e| e.name().is_none()) {
        Some(entry) => {
            entry.set_value(value);
            // a parsed entry prints the *text* it came from, not its value
            if let Some(format) = entry.format_mut() {
                format.value_repr = text;
            }
        }
        None => node.push(KdlEntry::new(value)),
    }
}

/// [`KdlValue`]'s own `Display` emits a bare string wherever KDL v2 allows one;
/// the documented config quotes its strings, and a toggle must not restyle them.
fn repr(value: &KdlValue) -> String {
    let KdlValue::String(s) = value else {
        return value.to_string();
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Build a node from KDL source, indented for `depth` and newline-terminated so
/// appending it does not run it onto the previous line.
fn new_node(doc: &KdlDocument, source: &str, depth: usize) -> KdlNode {
    let indent = "    ".repeat(depth);
    // A file that does not end in a newline would otherwise swallow the node.
    let separator = if doc.nodes().is_empty() || doc.to_string().ends_with('\n') {
        ""
    } else {
        "\n"
    };
    KdlNode::parse(&format!("{separator}{indent}{source}\n"))
        .expect("generated KDL node source is always valid")
}

/// Give a node a `{ … }` block if it has none, with a space before the brace.
fn open_children(node: &mut KdlNode) {
    if node.children().is_none()
        && let Some(format) = node.format_mut()
        && format.before_children.is_empty()
    {
        format.before_children = " ".to_string();
    }
}

/// Keep the file's existing permissions; a fresh one is owner-only, because
/// `proxy.key` is a shared secret.
fn mode_for(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Ok(meta) = std::fs::metadata(path) {
            return meta.permissions().mode() & 0o777;
        }
    }
    let _ = path;
    crate::store::FILE_MODE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::tests_support::temp_dir;

    const COMMENTED: &str = r#"// subbier config — hand-written, and it had better stay that way.

proxy {
    enabled #true
    // key "some-secret"        // required when bind is not loopback
    strategy "lowest-usage"     // lowest-usage | highest-usage | round-robin
    auto-switch #true
}

// how often we poll
poll {
    interval "60s"
}

ui {
    warn-pct 75
    critical-pct 90
}
"#;

    fn set_str(text: &str, key: &str, value: KdlValue) -> String {
        let mut doc: KdlDocument = text.parse().unwrap();
        set_in(&mut doc, key, value).unwrap();
        doc.to_string()
    }

    /// The whole point of the module: one line changes and every other byte survives.
    #[test]
    fn setting_a_key_changes_that_line_and_nothing_else() {
        let dir = temp_dir("config-write-roundtrip");
        let path = dir.join("config.kdl");
        std::fs::write(&path, COMMENTED).unwrap();

        set(
            &path,
            "proxy.strategy",
            KdlValue::String("round-robin".into()),
        )
        .unwrap();
        set(&path, "ui.critical-pct", KdlValue::Integer(95)).unwrap();

        let expected = COMMENTED
            .replace(r#"strategy "lowest-usage""#, r#"strategy "round-robin""#)
            .replace("critical-pct 90", "critical-pct 95");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            expected,
            "the file changed in more than the two values set"
        );

        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.proxy.strategy, crate::StrategyKind::RoundRobin);
        assert_eq!(config.ui.critical_pct, 95.0);
    }

    #[test]
    fn every_value_shape_round_trips_back_through_the_parser() {
        let after = set_str(COMMENTED, "proxy.auto-switch", KdlValue::Bool(false));
        assert!(!Config::parse(&after).unwrap().proxy.auto_switch);

        let after = set_str(COMMENTED, "ui.warn-pct", KdlValue::Integer(60));
        assert_eq!(Config::parse(&after).unwrap().ui.warn_pct, 60.0);

        let after = set_str(
            COMMENTED,
            "sub.claude:acct.label",
            KdlValue::String("work \"laptop\"".into()),
        );
        assert!(after.contains(r#"label "work \"laptop\"""#), "{after}");
        assert_eq!(
            Config::parse(&after)
                .unwrap()
                .sub_label(&crate::SubKey("claude:acct".into())),
            Some("work \"laptop\"")
        );
    }

    #[test]
    fn a_missing_key_or_block_is_created_where_it_belongs() {
        let after = set_str(COMMENTED, "proxy.sticky", KdlValue::Bool(true));
        assert_eq!(Config::parse(&after).unwrap().proxy.sticky, Some(true));
        let doc: KdlDocument = after.parse().unwrap();
        assert!(
            doc.get("proxy")
                .unwrap()
                .children()
                .unwrap()
                .get("sticky")
                .is_some(),
            "the new key landed inside the proxy block, not at top level"
        );
        assert!(doc.get("sticky").is_none());

        let after = set_str(COMMENTED, "history.retain-days", KdlValue::Integer(30));
        assert_eq!(Config::parse(&after).unwrap().history.retain_days, 30);
        assert!(
            after.contains("history {\n    retain-days 30\n}"),
            "unexpected layout:\n{after}"
        );
        assert!(after.contains("// subbier config"));
    }

    #[test]
    fn sub_overrides_are_addressed_by_their_key() {
        let after = set_str(
            COMMENTED,
            "sub.codex:4575f150-abc.enabled",
            KdlValue::Bool(false),
        );
        assert!(
            !Config::parse(&after)
                .unwrap()
                .sub_enabled(&crate::SubKey("codex:4575f150-abc".into()))
        );

        // a second key on the same sub reuses that node...
        let after = set_str(
            &after,
            "sub.codex:4575f150-abc.label",
            KdlValue::String("work".into()),
        );
        assert_eq!(after.matches("sub \"codex:4575f150-abc\"").count(), 1);

        // ...and a different sub, whose key contains dots, gets its own
        let after = set_str(
            &after,
            "sub.claude:me@ex.example.com.enabled",
            KdlValue::Bool(false),
        );
        let config = Config::parse(&after).unwrap();
        assert_eq!(config.subs.len(), 2);
        assert_eq!(
            config.sub_label(&crate::SubKey("codex:4575f150-abc".into())),
            Some("work")
        );
        assert!(!config.sub_enabled(&crate::SubKey("claude:me@ex.example.com".into())));
    }

    #[test]
    fn a_nonsense_key_is_rejected_and_writes_nothing() {
        let mut doc = KdlDocument::new();
        for key in ["", "proxy.", ".enabled", "proxy..enabled", "sub.only-a-key"] {
            assert!(
                set_in(&mut doc, key, KdlValue::Bool(true)).is_err(),
                "{key:?}"
            );
        }
        assert_eq!(doc.to_string(), "");
    }

    #[test]
    fn writing_a_missing_file_creates_one_holding_only_that_key() {
        let dir = temp_dir("config-write-new");
        let path = dir.join("config.kdl");
        set(&path, "proxy.enabled", KdlValue::Bool(false)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "proxy {\n    enabled #false\n}\n"
        );

        let config = Config::load_from(&path).unwrap();
        assert!(!config.proxy.enabled);
        assert_eq!(config.proxy.bind, crate::config::DEFAULT_BIND);
    }

    #[test]
    fn a_file_without_a_trailing_newline_gains_one_rather_than_a_mangled_node() {
        let dir = temp_dir("config-write-newline");
        let path = dir.join("config.kdl");
        std::fs::write(&path, "proxy {\n    enabled #true\n}").unwrap();

        set(&path, "history.retain-days", KdlValue::Integer(3)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "proxy {\n    enabled #true\n}\nhistory {\n    retain-days 3\n}\n"
        );
    }

    /// `proxy.key` is a shared secret, but a mode the user chose is theirs.
    #[cfg(unix)]
    #[test]
    fn a_new_config_is_owner_only_and_an_existing_mode_is_kept() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("config-write-mode");
        let path = dir.join("config.kdl");
        let mode = || std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;

        set(&path, "proxy.enabled", KdlValue::Bool(true)).unwrap();
        assert_eq!(mode(), 0o600);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        set(&path, "proxy.enabled", KdlValue::Bool(false)).unwrap();
        assert_eq!(mode(), 0o644);
    }
}
