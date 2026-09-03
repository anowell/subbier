//! On-disk state under `$SUBBIER_HOME` (else `~/.subbier`): `config.kdl` (user
//! intent), `subs.json` (credentials, 0600), `state.db` (time series) and
//! `transcripts.db` (disposable Codex chain state, 0600).

pub mod creds;
pub mod db;
pub mod transcripts;

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::error::Result;

#[cfg(unix)]
pub const DIR_MODE: u32 = 0o700;

#[cfg(unix)]
pub const FILE_MODE: u32 = 0o600;

/// The directory subbier owns. Does **not** create it; see [`ensure_home`].
#[must_use]
pub fn home() -> PathBuf {
    match std::env::var_os("SUBBIER_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".subbier"),
    }
}

/// [`home`], created if absent with mode 0700.
pub fn ensure_home() -> Result<PathBuf> {
    let dir = home();
    ensure_dir(&dir)?;
    Ok(dir)
}

/// Create `dir` (and parents) if absent, and clamp it to owner-only.
pub(crate) fn ensure_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(dir)?.permissions().mode() & 0o777;
        if mode != DIR_MODE {
            fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))?;
        }
    }
    Ok(())
}

/// Write `contents` to `path` via a temp file in the *same* directory, so the
/// rename is atomic; `mode` applies from creation, never briefly world-readable.
pub(crate) fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    ensure_dir(dir)?;

    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!(
        "{name}.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));

    let result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(mode);
        }
        let mut file = options.open(&temp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);

        fs::rename(&temp, path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    let _ = mode; // unused off unix
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn write_atomic_creates_an_owner_only_file_and_leaves_no_temp() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::store::tests_support::temp_dir("write-atomic");
        let path = dir.join("nested").join("secret.json");
        write_atomic(&path, b"{}", FILE_MODE).unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"{}");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            FILE_MODE
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            DIR_MODE
        );

        write_atomic(&path, b"[]", FILE_MODE).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"[]");
        let strays: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "left temp files behind: {strays:?}");
    }
}

/// Scratch directories for this crate's own tests; `tempfile` is deliberately
/// not a dependency.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    pub(crate) struct TempDir(PathBuf);

    impl std::ops::Deref for TempDir {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn temp_dir(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "libsubby-{tag}-{}-{n}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}
