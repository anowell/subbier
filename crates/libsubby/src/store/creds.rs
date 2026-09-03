//! `~/.subbier/subs.json`, mode 0600 — the credential store.
//!
//! subbier never writes refreshed tokens back to `~/.codex/auth.json` or the
//! Keychain; this file is where its own fresher copy lives.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::store::{FILE_MODE, write_atomic};

/// An alias, not a second struct, so persisted and in-memory shapes cannot drift.
pub type StoredSub = crate::model::Sub;

/// `subs` is a *field*, not the root array, so the format has somewhere to grow.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    subs: Vec<StoredSub>,
}

/// A missing file is an empty vec; a malformed one is an error, rather than a
/// silent wipe on the next save.
pub fn load_from(path: &Path) -> Result<Vec<StoredSub>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let store: Store = serde_json::from_str(&text)?;
    Ok(store.subs)
}

/// Replace the stored subscriptions, atomically, at mode 0600.
pub fn save_to(path: &Path, subs: &[StoredSub]) -> Result<()> {
    let store = Store {
        subs: subs.to_vec(),
    };
    let mut json = serde_json::to_vec_pretty(&store)?;
    json.push(b'\n');
    write_atomic(path, &json, FILE_MODE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CredentialSource, Credentials, Provider, SubKey, Tokens};
    use crate::store::{DIR_MODE, tests_support::temp_dir};
    use jiff::Timestamp;

    fn sample(n: u32) -> StoredSub {
        StoredSub {
            key: SubKey::new(Provider::Codex, format!("acct-{n}")),
            provider: Provider::Codex,
            label: format!("work-{n}"),
            credentials: Credentials {
                plan: None,
                account_id: Some(format!("acct-{n}")),
                email: Some(format!("me{n}@example.com")),
                tokens: Tokens {
                    access: format!("access-{n}"),
                    refresh: Some(format!("refresh-{n}")),
                    expires_at: Some(Timestamp::from_second(1_700_000_000).unwrap()),
                },
                source: CredentialSource::Keychain,
            },
        }
    }

    #[test]
    fn a_missing_file_loads_as_an_empty_vec() {
        let dir = temp_dir("creds-missing");
        assert_eq!(load_from(&dir.join("subs.json")).unwrap(), Vec::new());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = temp_dir("creds-roundtrip");
        let file = dir.join("nested").join("subs.json");
        let subs = vec![sample(1), sample(2)];

        save_to(&file, &subs).unwrap();
        assert_eq!(load_from(&file).unwrap(), subs);

        save_to(&file, &subs[..1]).unwrap();
        assert_eq!(load_from(&file).unwrap(), subs[..1].to_vec());
    }

    #[cfg(unix)]
    #[test]
    fn saving_tightens_the_file_to_0600_and_its_directory_to_0700() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("creds-modes");
        let file = dir.join("home").join("subs.json");
        save_to(&file, &[sample(1)]).unwrap();

        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), FILE_MODE);
        assert_eq!(mode(file.parent().unwrap()), DIR_MODE);

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        save_to(&file, &[sample(1)]).unwrap();
        assert_eq!(mode(&file), FILE_MODE);
    }

    #[test]
    fn a_malformed_file_is_an_error_rather_than_a_silent_wipe() {
        let dir = temp_dir("creds-malformed");
        let file = dir.join("subs.json");
        std::fs::write(&file, "{\"subs\": \"not an array\"}").unwrap();
        assert!(load_from(&file).is_err());

        std::fs::write(&file, "this is not json at all").unwrap();
        assert!(load_from(&file).is_err());
    }

    #[test]
    fn the_on_disk_shape_is_the_documented_one() {
        let dir = temp_dir("creds-shape");
        let file = dir.join("subs.json");
        save_to(&file, &[sample(7)]).unwrap();

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let sub = &value["subs"][0];
        for field in [
            "key",
            "provider",
            "label",
            "account_id",
            "email",
            "access",
            "refresh",
            "expires_at",
            "source",
        ] {
            assert!(sub.get(field).is_some(), "missing {field} in {value}");
        }
        assert_eq!(sub["key"], "codex:acct-7");
    }
}
