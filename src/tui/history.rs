//! Persistent recent-input history (search / filter / highlight terms), loaded
//! at startup and saved on exit — the zerch analogue of less's `~/.lesshst`.
//!
//! Location (first match wins):
//!   1. `$ZERCH_HISTFILE`             — explicit path; `-` or empty disables persistence
//!   2. `$XDG_STATE_HOME/zerch/history`
//!   3. `~/.local/state/zerch/history`
//!
//! History is machine-local, regenerable *state*, so it lives under
//! `$XDG_STATE_HOME` rather than a `~` dotfile. The file is one term per line,
//! newest first, capped at [`HISTORY_MAX`]. Terms are single-line (the input
//! prompt submits on Enter and trims), so a plain line-per-term format is safe.
//!
//! All I/O here is best-effort: a missing, unreadable, or unwritable history
//! file must never disrupt the viewer.

use std::path::{Path, PathBuf};

/// Cap on remembered terms (shared across search / filter / highlight).
pub const HISTORY_MAX: usize = 100;

/// Resolve the history file path, or `None` when persistence is disabled
/// (`$ZERCH_HISTFILE` set to `-` or empty) or no home directory is known.
fn resolve_path() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("ZERCH_HISTFILE") {
        let s = v.to_string_lossy();
        let t = s.trim();
        if t.is_empty() || t == "-" {
            return None; // explicitly disabled
        }
        return Some(PathBuf::from(t));
    }
    // $XDG_STATE_HOME, else ~/.local/state (per the XDG Base Directory spec).
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))?;
    Some(base.join("zerch").join("history"))
}

/// Load persisted history (newest first, deduplicated, capped). Empty when
/// disabled or the file is missing/unreadable.
pub fn load() -> Vec<String> {
    match resolve_path() {
        Some(p) => load_from(&p),
        None => Vec::new(),
    }
}

/// Persist `history` (already newest-first) to the resolved path. No-op when
/// disabled; silently ignores any I/O error.
pub fn save(history: &[String]) {
    if let Some(p) = resolve_path() {
        let _ = save_to(&p, history);
    }
}

fn load_from(path: &Path) -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for line in contents.lines() {
        let t = line.trim();
        if t.is_empty() || out.iter().any(|e| e == t) {
            continue; // skip blanks and duplicates (in case the file was hand-edited)
        }
        out.push(t.to_string());
        if out.len() >= HISTORY_MAX {
            break;
        }
    }
    out
}

fn save_to(path: &Path, history: &[String]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut buf = String::new();
    for t in history.iter().take(HISTORY_MAX) {
        let t = t.trim();
        if t.is_empty() {
            continue;
        }
        buf.push_str(t);
        buf.push('\n');
    }
    // Write to a per-process temp file in the same directory, then rename, so a
    // reader never sees a half-written file and concurrent exits don't clobber.
    let tmp = path.with_file_name(format!(".zerch-hist.{}.tmp", std::process::id()));
    write_private(&tmp, buf.as_bytes())?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp); // don't leave the temp behind
            Err(e)
        }
    }
}

// Write with owner-only permissions (0600) on Unix — search terms can be
// sensitive, matching less's private ~/.lesshst.
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
    fn round_trips_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("history"); // parent auto-created
        let terms = vec!["error".to_string(), "warn".to_string(), "svc=api".to_string()];
        save_to(&path, &terms).unwrap();
        assert_eq!(load_from(&path), terms);
    }

    #[test]
    fn load_dedups_and_caps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        let mut lines = String::from("a\n\nb\na\n"); // blank + duplicate 'a'
        for i in 0..HISTORY_MAX + 20 {
            lines.push_str(&format!("t{i}\n"));
        }
        std::fs::write(&path, lines).unwrap();
        let got = load_from(&path);
        assert_eq!(&got[..2], &["a".to_string(), "b".to_string()]);
        assert_eq!(got.len(), HISTORY_MAX);
        // No duplicates survived.
        let mut sorted = got.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), got.len());
    }

    #[test]
    fn save_caps_to_max() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        let terms: Vec<String> = (0..HISTORY_MAX + 50).map(|i| format!("t{i}")).collect();
        save_to(&path, &terms).unwrap();
        assert_eq!(load_from(&path).len(), HISTORY_MAX);
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_from(&dir.path().join("nope")).is_empty());
    }

    #[test]
    fn env_override_and_disable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hist");

        // ZERCH_HISTFILE points the public API at our temp file.
        std::env::set_var("ZERCH_HISTFILE", &path);
        save(&["alpha".to_string(), "beta".to_string()]);
        assert_eq!(load(), vec!["alpha".to_string(), "beta".to_string()]);

        // "-" disables persistence: save is a no-op and load returns empty.
        std::env::set_var("ZERCH_HISTFILE", "-");
        save(&["should-not-write".to_string()]);
        assert!(load().is_empty());

        // The disabled save must not have touched the real file.
        std::env::set_var("ZERCH_HISTFILE", &path);
        assert_eq!(load(), vec!["alpha".to_string(), "beta".to_string()]);

        std::env::remove_var("ZERCH_HISTFILE");
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        save_to(&path, &["secret-token".to_string()]).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
