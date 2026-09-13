//! Per-file view sessions: remember each file's filters, highlights, bookmarks,
//! hidden lines, search, and last cursor line, so reopening the file restores
//! exactly where you left off. The analogue of vim's per-file marks in viminfo.
//!
//! A session is the `--view` string keyed by the file's canonical path. Files
//! live under `$XDG_STATE_HOME/zerch/sessions/` (or `~/.local/state/…`), one per
//! file, named by a hash of the path, `0600`, written atomically. Set
//! `ZERCH_NO_SESSION=1` to disable both saving and restoring. All I/O is
//! best-effort and never disrupts the viewer.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

fn disabled() -> bool {
    match std::env::var("ZERCH_NO_SESSION") {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    }
}

fn sessions_dir() -> Option<PathBuf> {
    if disabled() {
        return None;
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))?;
    Some(base.join("zerch").join("sessions"))
}

fn file_for(dir: &Path, key: &Path) -> PathBuf {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    dir.join(format!("{:016x}.view", h.finish()))
}

/// Load the saved view string for `key` (a canonical file path), if any.
pub fn load(key: &Path) -> Option<String> {
    load_from(&sessions_dir()?, key)
}

/// Save `view` (a `--view` string) for `key`. An empty view clears any session.
pub fn save(key: &Path, view: &str) {
    if let Some(dir) = sessions_dir() {
        let _ = save_to(&dir, key, view);
    }
}

fn load_from(dir: &Path, key: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(file_for(dir, key)).ok()?;
    let mut lines = contents.lines();
    // First line is the stored path — guards against a hash collision mapping two
    // different files to the same session file.
    if lines.next()? != key.to_string_lossy() {
        return None;
    }
    match lines.next() {
        Some(v) if !v.is_empty() => Some(v.to_string()),
        _ => None,
    }
}

fn save_to(dir: &Path, key: &Path, view: &str) -> std::io::Result<()> {
    let f = file_for(dir, key);
    if view.is_empty() {
        let _ = std::fs::remove_file(&f); // nothing to restore → drop any stale session
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let body = format!("{}\n{}\n", key.to_string_lossy(), view);
    let tmp = f.with_file_name(format!(".zerch-sess.{}.tmp", std::process::id()));
    write_private(&tmp, body.as_bytes())?;
    match std::fs::rename(&tmp, &f) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

// Owner-only (0600) on Unix — a session reveals which files you viewed and your
// search terms.
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.flush()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let key = Path::new("/logs/app.log");
        save_to(dir.path(), key, "filters=error&selected=42").unwrap();
        assert_eq!(
            load_from(dir.path(), key).as_deref(),
            Some("filters=error&selected=42")
        );
        // A different path does not read the same session.
        assert!(load_from(dir.path(), Path::new("/logs/other.log")).is_none());
    }

    #[test]
    fn empty_view_clears_session() {
        let dir = tempfile::tempdir().unwrap();
        let key = Path::new("/logs/app.log");
        save_to(dir.path(), key, "selected=5").unwrap();
        assert!(load_from(dir.path(), key).is_some());
        save_to(dir.path(), key, "").unwrap();
        assert!(load_from(dir.path(), key).is_none());
    }

    #[test]
    fn missing_session_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_from(dir.path(), Path::new("/nope")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn session_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let key = Path::new("/logs/app.log");
        save_to(dir.path(), key, "search=secret").unwrap();
        let mode = std::fs::metadata(file_for(dir.path(), key))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
