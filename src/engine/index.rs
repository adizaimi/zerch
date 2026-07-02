//! The Tier-1 in-memory index: `mmap` the log file once, scan it for line
//! boundaries, and record per-line metadata (timestamp + level). Line *content*
//! is never copied — [`LineIndex::get_line`] returns a slice of the mapping
//! (ANSI-stripped on the fly), so opening a multi-GB file is effectively instant
//! and memory stays bounded to ~17 bytes of metadata per line.
//!
//! This replaces the Node app's SQLite `log_index` table for the common case;
//! the optional persistent SQLite/FTS5 tier is layered on later (Phase 9).

use crate::engine::ansi::strip_ansi;
use crate::engine::parse::{parse_line, NaiveZone, ParseOptions};
use crate::levels::{Level, LEVEL_NONE};
use memmap2::Mmap;
use std::borrow::Cow;
use std::fs::File;
use std::io;
use std::path::Path;

const NO_TS: i64 = i64::MIN;

#[derive(Clone, Copy)]
pub struct IndexOptions {
    /// Interpretation for offset-less timestamps. Defaults to `Utc` (deterministic);
    /// the CLI may override (e.g. from `ZERCHER_NAIVE_TZ`) in a later phase.
    pub naive_zone: NaiveZone,
    /// Year for year-less formats (BSD syslog). `None` → current UTC year.
    pub default_year: Option<i32>,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            naive_zone: NaiveZone::Utc,
            default_year: None,
        }
    }
}

pub struct LineIndex {
    mmap: Option<Mmap>,
    /// `starts.len() == total_lines + 1`; `starts[i]` is the byte offset of line
    /// `i+1`, and `starts[total_lines]` is the file length (sentinel end).
    starts: Vec<u64>,
    ts: Vec<i64>, // NO_TS = none
    levels: Vec<Level>,
    max_line_len: usize,
}

impl LineIndex {
    /// Open and index a file. `on_progress` is called with a 0..=100 percentage.
    pub fn open<F: FnMut(f64)>(
        path: &Path,
        opts: IndexOptions,
        mut on_progress: F,
    ) -> io::Result<LineIndex> {
        let file = File::open(path)?;
        let len = file.metadata()?.len() as usize;

        // mmap rejects a zero-length mapping on some platforms; handle empty files
        // (and empty maps) explicitly.
        let mmap = if len == 0 {
            None
        } else {
            Some(unsafe { Mmap::map(&file)? })
        };
        let bytes: &[u8] = mmap.as_ref().map(|m| &m[..]).unwrap_or(&[]);

        // Line-start offsets (fast: memchr over the whole mapping).
        let mut starts: Vec<u64> = Vec::new();
        if len > 0 {
            starts.push(0);
        }
        for idx in memchr::memchr_iter(b'\n', bytes) {
            if idx + 1 < len {
                starts.push((idx + 1) as u64);
            }
        }
        let total = starts.len();
        starts.push(len as u64); // sentinel end

        // Per-line timestamp + level.
        let popts = ParseOptions {
            default_year: opts.default_year,
            naive_zone: Some(opts.naive_zone),
        };
        let mut ts = Vec::with_capacity(total);
        let mut levels = Vec::with_capacity(total);
        let mut max_line_len = 0usize;
        for i in 0..total {
            let raw = trim_eol(&bytes[starts[i] as usize..starts[i + 1] as usize]);
            let text = clean_str(raw);
            let parsed = parse_line(&text, popts);
            ts.push(parsed.ts.unwrap_or(NO_TS));
            levels.push(parsed.level);
            max_line_len = max_line_len.max(text.chars().count());
            if i % 65_536 == 0 {
                on_progress(starts[i] as f64 / len.max(1) as f64 * 100.0);
            }
        }
        on_progress(100.0);

        Ok(LineIndex {
            mmap,
            starts,
            ts,
            levels,
            max_line_len,
        })
    }

    pub fn total_lines(&self) -> usize {
        self.starts.len() - 1
    }

    pub fn max_line_length(&self) -> usize {
        self.max_line_len
    }

    fn bytes(&self) -> &[u8] {
        self.mmap.as_ref().map(|m| &m[..]).unwrap_or(&[])
    }

    // Raw bytes of a 1-based line, EOL trimmed. Empty for out-of-range `n`.
    fn raw_line(&self, n: usize) -> &[u8] {
        if n == 0 || n > self.total_lines() {
            return &[];
        }
        let b = self.bytes();
        trim_eol(&b[self.starts[n - 1] as usize..self.starts[n] as usize])
    }

    /// Content of a 1-based line, ANSI-stripped. Borrows the mapping when the line
    /// is valid UTF-8 with no escape sequences (zero-copy); owned otherwise.
    pub fn get_line(&self, n: usize) -> Cow<'_, str> {
        clean_str(self.raw_line(n))
    }

    /// Lines `[start, end]` (1-based, inclusive), clamped to the file.
    pub fn get_range(&self, start: usize, end: usize) -> Vec<Cow<'_, str>> {
        let lo = start.max(1);
        let hi = end.min(self.total_lines());
        (lo..=hi).map(|n| self.get_line(n)).collect()
    }

    /// Timestamp (epoch ms) of a 1-based line, if it carried one.
    pub fn ts_of(&self, n: usize) -> Option<i64> {
        match self.ts.get(n.wrapping_sub(1)) {
            Some(&v) if v != NO_TS => Some(v),
            _ => None,
        }
    }

    /// Level code of a 1-based line (0 = none).
    pub fn level_of(&self, n: usize) -> Level {
        self.levels.get(n.wrapping_sub(1)).copied().unwrap_or(LEVEL_NONE)
    }
}

fn trim_eol(mut raw: &[u8]) -> &[u8] {
    if raw.last() == Some(&b'\n') {
        raw = &raw[..raw.len() - 1];
    }
    if raw.last() == Some(&b'\r') {
        raw = &raw[..raw.len() - 1];
    }
    raw
}

fn clean_str(raw: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(raw) {
        Ok(s) => strip_ansi(s),
        Err(_) => Cow::Owned(strip_ansi(&String::from_utf8_lossy(raw)).into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn write_temp(bytes: &[u8]) -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "zercher_idx_{}_{}.log",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let mut f = File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    fn open(bytes: &[u8]) -> LineIndex {
        let p = write_temp(bytes);
        let idx = LineIndex::open(&p, IndexOptions::default(), |_| {}).unwrap();
        let _ = std::fs::remove_file(&p);
        idx
    }

    #[test]
    fn indexes_lines_ts_and_levels() {
        let mut content = Vec::new();
        content.extend_from_slice(b"line1 plain\n");
        content.extend_from_slice(b"2026-06-20T12:00:00Z INFO started\n");
        content.extend_from_slice(b"2026-06-20T12:00:01Z ERROR boom\n");
        content.extend_from_slice(br#"{"ts":"2026-06-20T12:00:02Z","level":"warn","msg":"hi"}"#);
        content.extend_from_slice(b"\n");
        // Colourised final line, no trailing newline.
        content.extend_from_slice(b"=== \x1b[31mFAIL\x1b[0m: test");

        let idx = open(&content);
        assert_eq!(idx.total_lines(), 5);
        assert_eq!(idx.get_line(1), "line1 plain");
        assert_eq!(idx.level_of(2), 3); // INFO
        assert_eq!(idx.level_of(3), 5); // ERROR
        assert_eq!(idx.level_of(4), 4); // WARN (from JSON)
        assert!(idx.ts_of(2).is_some());
        assert_eq!(idx.ts_of(2), idx.ts_of(2)); // stable
        assert!(idx.ts_of(1).is_none()); // plain line, no ts
        // ANSI stripped on read; final line has no newline.
        assert_eq!(idx.get_line(5), "=== FAIL: test");
        // longest line is the JSON one (line 4).
        assert_eq!(idx.max_line_length(), idx.get_line(4).chars().count());
    }

    #[test]
    fn get_range_is_clamped_and_inclusive() {
        let idx = open(b"a\nb\nc\nd\n");
        let r = idx.get_range(2, 3);
        assert_eq!(r, vec![Cow::from("b"), Cow::from("c")]);
        // Clamped to bounds.
        let all = idx.get_range(0, 999);
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn crlf_line_endings() {
        let idx = open(b"one\r\ntwo\r\n");
        assert_eq!(idx.total_lines(), 2);
        assert_eq!(idx.get_line(1), "one");
        assert_eq!(idx.get_line(2), "two");
    }

    #[test]
    fn edge_cases_empty_and_no_final_newline() {
        let empty = open(b"");
        assert_eq!(empty.total_lines(), 0);
        assert_eq!(empty.get_range(1, 10).len(), 0);
        assert_eq!(empty.get_line(1), ""); // out of range → empty

        let single_nl = open(b"\n");
        assert_eq!(single_nl.total_lines(), 1);
        assert_eq!(single_nl.get_line(1), "");

        let no_nl = open(b"only line");
        assert_eq!(no_nl.total_lines(), 1);
        assert_eq!(no_nl.get_line(1), "only line");
    }
}
