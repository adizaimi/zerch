//! The Tier-1 in-memory index: `mmap` the log file once, scan it for line
//! boundaries, and record per-line metadata (timestamp + level). Line *content*
//! is never copied — [`LineIndex::get_line`] returns a slice of the mapping
//! (ANSI-stripped on the fly), so opening a multi-GB file is effectively instant
//! and memory stays bounded to a small, fixed amount of metadata per line.
//!
//! Indexing is split in two: finding line boundaries (`open`) is fast and
//! synchronous, so the file is immediately scrollable/searchable by content.
//! Per-line timestamp + level extraction is comparatively expensive (JSON
//! lines need a full parse) and runs on a dedicated background thread,
//! chunk by chunk, so it never blocks the caller. Until a line's chunk has
//! landed, it simply reads as "no timestamp / no level" — indistinguishable
//! from a real miss, and self-correcting on the next read once the
//! background thread catches up.
//!
//! This replaces the Node app's SQLite `log_index` table for the common case;
//! the optional persistent SQLite/FTS5 tier is layered on later (Phase 9).

use crate::engine::ansi::strip_ansi;
use crate::engine::parse::{parse_line, NaiveZone, ParseOptions, Parsed};
use crate::levels::{Level, LEVEL_NONE};
use memmap2::Mmap;
use rayon::prelude::*;
use std::borrow::Cow;
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

const NO_TS: i64 = i64::MIN;

// Lines processed per background-indexing step. Small enough that the first
// screenful of a huge file gets its colours within a chunk or two of opening;
// large enough that thread-wakeup/rayon-dispatch overhead stays negligible.
const META_CHUNK_LINES: usize = 32_768;

/// Line-start byte offsets. Files whose length fits in `u32` (`< 4 GiB`) use a
/// half-size Vec; larger files fall back to `u64`.
enum LineStarts {
    Small(Vec<u32>),
    Large(Vec<u64>),
}

impl LineStarts {
    fn from_u64s(file_len: u64, xs: Vec<u64>) -> Self {
        if file_len <= u32::MAX as u64 {
            Self::Small(xs.into_iter().map(|v| v as u32).collect())
        } else {
            Self::Large(xs)
        }
    }
    fn push_end(&mut self, len: u64) {
        match self {
            Self::Small(v) => v.push(len as u32),
            Self::Large(v) => v.push(len),
        }
    }
    /// Release growth-strategy over-allocation. Called once after indexing since
    /// the Vecs are built to their final size and never grow again.
    fn shrink(&mut self) {
        match self {
            Self::Small(v) => v.shrink_to_fit(),
            Self::Large(v) => v.shrink_to_fit(),
        }
    }
    fn get(&self, i: usize) -> u64 {
        match self {
            Self::Small(v) => v[i] as u64,
            Self::Large(v) => v[i],
        }
    }
    fn len(&self) -> usize {
        match self {
            Self::Small(v) => v.len(),
            Self::Large(v) => v.len(),
        }
    }
}

#[derive(Clone, Copy)]
pub struct IndexOptions {
    /// Interpretation for offset-less timestamps. Defaults to `Utc` (deterministic);
    /// the CLI may override (e.g. from `ZERCH_NAIVE_TZ`) in a later phase.
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
    starts: LineStarts,
    /// Per-line timestamp (epoch ms), `NO_TS` for "no timestamp" *or* "not
    /// indexed yet" — the two are indistinguishable and that's fine, since the
    /// latter self-corrects the moment the background thread reaches this line.
    ts: Vec<AtomicI64>,
    /// Per-line level code, `LEVEL_NONE` for "no level" *or* "not indexed yet".
    levels: Vec<AtomicU8>,
    /// How many lines (from the start) the background thread has finished.
    /// Chunks are processed strictly in ascending order by a single thread, so
    /// this count doubles as a completion watermark. `fetch_add` uses `Release`
    /// after a chunk's cells are written; readers use `Acquire` so a caller that
    /// observes completion is guaranteed to see every cell's final value.
    indexed_count: AtomicUsize,
    max_line_len: usize,
}

impl LineIndex {
    /// Open a file: mmap it and find line boundaries synchronously (fast — the
    /// file is immediately scrollable/searchable by content), then kick off
    /// per-line timestamp/level extraction on a background thread. Poll
    /// [`metadata_progress`]/[`metadata_complete`] to track that background work.
    pub fn open(path: &Path, opts: IndexOptions) -> io::Result<Arc<LineIndex>> {
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

        // Line-start offsets. memchr is memory-bandwidth bound, so we chunk the
        // mapping across rayon workers to saturate DRAM instead of a single core.
        let raw_starts = find_line_starts_parallel(bytes);
        let total = raw_starts.len();

        // Longest line in bytes (incl. terminator) — a cheap upper bound for the
        // horizontal-scroll limit; depends only on line starts, not parsed
        // metadata, so it's part of the fast structural phase.
        let max_line_len = (0..total)
            .map(|i| {
                let end = if i + 1 < total { raw_starts[i + 1] } else { len as u64 };
                (end - raw_starts[i]) as usize
            })
            .max()
            .unwrap_or(0);

        let mut starts = LineStarts::from_u64s(len as u64, raw_starts);
        starts.push_end(len as u64); // sentinel end
        starts.shrink();

        let ts: Vec<AtomicI64> = (0..total).map(|_| AtomicI64::new(NO_TS)).collect();
        let levels: Vec<AtomicU8> = (0..total).map(|_| AtomicU8::new(LEVEL_NONE)).collect();

        let index = Arc::new(LineIndex {
            mmap,
            starts,
            ts,
            levels,
            indexed_count: AtomicUsize::new(0),
            max_line_len,
        });

        if total > 0 {
            let bg = Arc::clone(&index);
            let popts = ParseOptions {
                default_year: opts.default_year,
                naive_zone: Some(opts.naive_zone),
            };
            thread::spawn(move || bg.index_metadata_in_background(popts));
        }

        Ok(index)
    }

    // Parse timestamp + level for every line, one chunk at a time, publishing
    // progress after each chunk. Each chunk is itself parsed in parallel across
    // the global rayon pool; only the chunk *sequencing* is single-threaded here.
    fn index_metadata_in_background(&self, popts: ParseOptions) {
        let total = self.total_lines();
        let bytes = self.bytes();
        let mut start = 0;
        while start < total {
            let end = (start + META_CHUNK_LINES).min(total);
            (start..end).into_par_iter().for_each(|i| {
                let lo = self.starts.get(i) as usize;
                let hi = self.starts.get(i + 1) as usize;
                let raw = trim_eol(&bytes[lo..hi]);
                let p = parse_meta(raw, popts);
                self.ts[i].store(p.ts.unwrap_or(NO_TS), Ordering::Relaxed);
                self.levels[i].store(p.level, Ordering::Relaxed);
            });
            self.indexed_count.fetch_add(end - start, Ordering::Release);
            start = end;
        }
    }

    /// Fraction (0.0..=1.0) of lines whose timestamp/level have been extracted.
    pub fn metadata_progress(&self) -> f64 {
        let total = self.total_lines();
        if total == 0 {
            return 1.0;
        }
        (self.indexed_count.load(Ordering::Acquire) as f64 / total as f64).min(1.0)
    }

    /// True once every line's timestamp/level has been extracted. `time_bounds`/
    /// `histogram` should only be called once this is true.
    pub fn metadata_complete(&self) -> bool {
        self.indexed_count.load(Ordering::Acquire) >= self.total_lines()
    }

    pub fn total_lines(&self) -> usize {
        self.starts.len().saturating_sub(1)
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
        trim_eol(&b[self.starts.get(n - 1) as usize..self.starts.get(n) as usize])
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

    /// Timestamp (epoch ms) of a 1-based line, if it carried one (or `None` if its
    /// background metadata hasn't landed yet — indistinguishable from a real miss,
    /// and self-correcting on the next call once it has).
    pub fn ts_of(&self, n: usize) -> Option<i64> {
        if n == 0 || n > self.total_lines() {
            return None;
        }
        let v = self.ts[n - 1].load(Ordering::Relaxed);
        (v != NO_TS).then_some(v)
    }

    /// Level code of a 1-based line (0 = none, or not indexed yet).
    pub fn level_of(&self, n: usize) -> Level {
        if n == 0 || n > self.total_lines() {
            LEVEL_NONE
        } else {
            self.levels[n - 1].load(Ordering::Relaxed)
        }
    }

    /// `(min_ts, max_ts, count)` over timestamped lines, or `None` if none carry a
    /// timestamp. Only meaningful once [`metadata_complete`] is true.
    pub fn time_bounds(&self) -> Option<(i64, i64, usize)> {
        let mut min = i64::MAX;
        let mut max = i64::MIN;
        let mut count = 0usize;
        for cell in &self.ts {
            let v = cell.load(Ordering::Relaxed);
            if v != NO_TS {
                if v < min {
                    min = v;
                }
                if v > max {
                    max = v;
                }
                count += 1;
            }
        }
        (count > 0).then_some((min, max, count))
    }

    /// Bucket the timestamped lines into `buckets` bins for the histogram: a count
    /// and the most-severe level per bucket. `None` if the file has no timestamps.
    /// Only meaningful once [`metadata_complete`] is true.
    pub fn histogram(&self, buckets: usize) -> Option<Histogram> {
        let (min_ts, max_ts, _) = self.time_bounds()?;
        let buckets = buckets.max(1);
        let span = (max_ts - min_ts + 1).max(1);
        let bucket_ms = ((span + buckets as i64 - 1) / buckets as i64).max(1);
        let mut counts = vec![0u64; buckets];
        let mut max_level = vec![LEVEL_NONE; buckets];
        for i in 0..self.ts.len() {
            let t = self.ts[i].load(Ordering::Relaxed);
            if t == NO_TS {
                continue;
            }
            let b = (((t - min_ts) / bucket_ms) as usize).min(buckets - 1);
            counts[b] += 1;
            let lvl = self.levels[i].load(Ordering::Relaxed);
            if lvl > max_level[b] {
                max_level[b] = lvl;
            }
        }
        Some(Histogram {
            min_ts,
            max_ts,
            bucket_ms,
            counts,
            max_level,
        })
    }
}

/// A time histogram: `counts[i]` lines fall in bucket `i` (`bucket_ms` wide,
/// starting at `min_ts`), with the most-severe level seen in that bucket.
pub struct Histogram {
    pub min_ts: i64,
    pub max_ts: i64,
    pub bucket_ms: i64,
    pub counts: Vec<u64>,
    pub max_level: Vec<Level>,
}

// Discover all line-start offsets (line 1 = 0; subsequent lines start right after
// each newline; a trailing newline does NOT produce a phantom empty last line).
// Chunks the buffer across rayon workers so memchr saturates memory bandwidth.
fn find_line_starts_parallel(bytes: &[u8]) -> Vec<u64> {
    let n = bytes.len();
    if n == 0 {
        return Vec::new();
    }
    // Tiny files: parallel split is pure overhead; do it inline.
    const MIN_CHUNK_BYTES: usize = 4 * 1024 * 1024; // 4 MiB
    let threads = rayon::current_num_threads().max(1);
    let chunk_count = (n / MIN_CHUNK_BYTES).max(1).min(threads);

    let mut out: Vec<u64> = Vec::with_capacity(1);
    out.push(0);

    if chunk_count == 1 {
        for i in memchr::memchr_iter(b'\n', bytes) {
            if i + 1 < n {
                out.push((i + 1) as u64);
            }
        }
        return out;
    }

    let chunk_size = n.div_ceil(chunk_count);
    // Each worker returns absolute "next line start" offsets in its chunk.
    let per_chunk: Vec<Vec<u64>> = (0..chunk_count)
        .into_par_iter()
        .map(|c| {
            let start = c * chunk_size;
            let end = ((c + 1) * chunk_size).min(n);
            let mut v = Vec::new();
            for i in memchr::memchr_iter(b'\n', &bytes[start..end]) {
                let abs = start + i + 1;
                if abs < n {
                    v.push(abs as u64);
                }
            }
            v
        })
        .collect();

    let total_extra: usize = per_chunk.iter().map(|v| v.len()).sum();
    out.reserve(total_extra);
    for v in per_chunk {
        out.extend(v);
    }
    out
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

// Parse a line's ts/level without scanning its whole length: JSON lines need the
// full (stripped) object; everything else only needs a short prefix.
fn parse_meta(raw: &[u8], opts: ParseOptions) -> Parsed {
    let leading = raw.iter().position(|&b| b != b' ' && b != b'\t');
    let is_json = matches!(leading, Some(i) if raw[i] == b'{');
    if is_json {
        parse_line(&clean_str(raw), opts)
    } else {
        let mut end = raw.len().min(256);
        while end < raw.len() && (raw[end] & 0xC0) == 0x80 {
            end += 1; // extend to the next UTF-8 char boundary
        }
        parse_line(&clean_str(&raw[..end]), opts)
    }
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
    use std::sync::atomic::{AtomicU64, Ordering as StdOrdering};

    fn write_temp(bytes: &[u8]) -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "zerch_idx_{}_{}.log",
            std::process::id(),
            N.fetch_add(1, StdOrdering::Relaxed)
        ));
        let mut f = File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    // Opens and blocks until background metadata indexing has finished, so
    // existing synchronous test assertions on level_of/ts_of/etc. keep working.
    fn open(bytes: &[u8]) -> Arc<LineIndex> {
        let p = write_temp(bytes);
        let idx = LineIndex::open(&p, IndexOptions::default()).unwrap();
        let _ = std::fs::remove_file(&p);
        while !idx.metadata_complete() {
            std::thread::yield_now();
        }
        idx
    }

    // Reference (sequential) implementation for cross-checking the parallel one.
    fn find_line_starts_seq(bytes: &[u8]) -> Vec<u64> {
        let n = bytes.len();
        if n == 0 {
            return Vec::new();
        }
        let mut out = vec![0u64];
        for i in memchr::memchr_iter(b'\n', bytes) {
            if i + 1 < n {
                out.push((i + 1) as u64);
            }
        }
        out
    }

    #[test]
    fn parallel_newline_scan_matches_sequential() {
        // Small: single-chunk fast path.
        for bytes in [
            &b""[..],
            &b"\n"[..],
            &b"a"[..],
            &b"a\n"[..],
            &b"a\nb\nc"[..],
            &b"a\r\nb\r\n"[..],
            &b"\n\n\n"[..],
        ] {
            assert_eq!(
                find_line_starts_parallel(bytes),
                find_line_starts_seq(bytes),
                "mismatch for {bytes:?}"
            );
        }
        // Large: forces the parallel branch (> MIN_CHUNK_BYTES).
        let mut big = Vec::with_capacity(8 * 1024 * 1024 + 3);
        for i in 0..200_000 {
            big.extend_from_slice(format!("line {i} contents here\n").as_bytes());
        }
        big.extend_from_slice(b"no trailing newline");
        assert_eq!(find_line_starts_parallel(&big), find_line_starts_seq(&big));
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
        // max_line_length is a byte-length upper bound (>= the longest line's chars).
        assert!(idx.max_line_length() >= idx.get_line(4).chars().count());
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
    #[ignore = "manual perf check: parallel vs sequential newline scan on ws.log"]
    fn bench_newline_scan() {
        let path = std::path::Path::new("/tmp/ws.log");
        if !path.exists() {
            eprintln!("skip: no /tmp/ws.log");
            return;
        }
        let f = File::open(path).unwrap();
        let mmap = unsafe { memmap2::Mmap::map(&f) }.unwrap();
        let bytes: &[u8] = &mmap[..];
        // Warm the page cache once so both runs read from RAM.
        let _ = find_line_starts_seq(bytes).len();

        let t = std::time::Instant::now();
        let seq = find_line_starts_seq(bytes);
        let t_seq = t.elapsed();

        let t = std::time::Instant::now();
        let par = find_line_starts_parallel(bytes);
        let t_par = t.elapsed();

        assert_eq!(seq, par, "parallel result differs from sequential");
        eprintln!(
            "{} bytes / {} lines · sequential {:?} · parallel {:?} ({} threads)",
            bytes.len(),
            seq.len(),
            t_seq,
            t_par,
            rayon::current_num_threads()
        );
    }

    #[test]
    fn line_starts_small_vs_large() {
        // Small path uses u32 halving for files under 4 GiB.
        let xs = vec![0u64, 100, 5000, 40000];
        let ls = LineStarts::from_u64s(u32::MAX as u64, xs.clone());
        assert!(matches!(ls, LineStarts::Small(_)));
        for (i, &v) in xs.iter().enumerate() {
            assert_eq!(ls.get(i), v);
        }
        // Large path is used once the file itself is >= 4 GiB.
        let ls = LineStarts::from_u64s(u32::MAX as u64 + 1, xs.clone());
        assert!(matches!(ls, LineStarts::Large(_)));
        for (i, &v) in xs.iter().enumerate() {
            assert_eq!(ls.get(i), v);
        }
    }

    #[test]
    #[ignore = "manual perf check against a local large file"]
    fn bench_metadata_footprint_ws() {
        let path = std::path::Path::new("/tmp/ws.log");
        if !path.exists() {
            eprintln!("skip: no /tmp/ws.log");
            return;
        }
        let idx = open(&std::fs::read(path).unwrap());
        let n = idx.total_lines();
        let starts_b = match &idx.starts {
            LineStarts::Small(v) => v.capacity() * 4,
            LineStarts::Large(v) => v.capacity() * 8,
        };
        let ts_b = idx.ts.capacity() * std::mem::size_of::<AtomicI64>();
        let levels_b = idx.levels.capacity() * std::mem::size_of::<AtomicU8>();
        let total = starts_b + ts_b + levels_b;
        eprintln!(
            "{n} lines · starts {} MiB · ts {} MiB · levels {} KiB · total {} MiB ({:.1} B/line)",
            starts_b / (1024 * 1024),
            ts_b / (1024 * 1024),
            levels_b / 1024,
            total / (1024 * 1024),
            total as f64 / n as f64
        );
    }

    #[test]
    #[ignore = "manual perf check against a local large file"]
    fn bench_open_ws() {
        let path = std::path::Path::new("/tmp/ws.log");
        if !path.exists() {
            eprintln!("skip: no /tmp/ws.log");
            return;
        }
        let t = std::time::Instant::now();
        let idx = LineIndex::open(path, IndexOptions::default()).unwrap();
        eprintln!("structural open of {} lines in {:?}", idx.total_lines(), t.elapsed());
        while !idx.metadata_complete() {
            std::thread::yield_now();
        }
        eprintln!("metadata fully indexed in {:?}", t.elapsed());
    }

    #[test]
    fn histogram_buckets_by_time_and_level() {
        let mut c = Vec::new();
        c.extend_from_slice(b"2026-06-20T00:00:00Z INFO a\n");
        c.extend_from_slice(b"2026-06-20T00:00:00Z ERROR b\n");
        c.extend_from_slice(b"no timestamp here\n");
        c.extend_from_slice(b"2026-06-20T02:00:00Z WARN c\n");
        let idx = open(&c);
        assert!(idx.time_bounds().is_some());
        let h = idx.histogram(2).unwrap();
        assert_eq!(h.counts.iter().sum::<u64>(), 3); // only timestamped lines
        assert_eq!(h.max_level[0], 5); // bucket 0 holds INFO+ERROR → ERROR

        let plain = open(b"a\nb\nc\n");
        assert!(plain.time_bounds().is_none());
        assert!(plain.histogram(4).is_none());
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

    #[test]
    fn metadata_progress_reaches_one() {
        let mut c = Vec::new();
        for i in 0..(META_CHUNK_LINES * 2 + 100) {
            c.extend_from_slice(format!("line {i}\n").as_bytes());
        }
        let idx = open(&c); // waits for completion internally
        assert!(idx.metadata_complete());
        assert_eq!(idx.metadata_progress(), 1.0);
    }
}
