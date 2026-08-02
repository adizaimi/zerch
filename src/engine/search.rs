//! Search over the Tier-1 [`LineIndex`]: a scan that reproduces the exact result
//! semantics of the Node SQLite engine (`search.ts` / `regexpFn.ts`), minus SQL.
//!
//! Semantics preserved verbatim:
//! - A stack of terms joins left-to-right; **AND binds tighter than OR**, so the
//!   stack is a disjunction of AND-groups (a new group starts at each `OR` term).
//! - Each term is literal (case-sensitive `contains`, or ASCII case-insensitive to
//!   match SQLite's ASCII-only `lower()`) or a `regex`; `negate` inverts it.
//! - An empty stack matches every line.
//! - A [`Scope`] AND-composes a time-range predicate (timestamped lines only) and a
//!   level-set predicate on top of the stack result.
//!
//! Regex uses the linear-time `regex` crate, so there is no worker pool / deadline:
//! a pathological pattern simply can't blow up. Invalid patterns surface as `Err`.

use crate::engine::index::LineIndex;
use crate::levels::Level;
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    And,
    Or,
}

#[derive(Clone, Debug)]
pub struct SearchTerm {
    pub term: String,
    /// How this term joins the running expression (ignored for the first term).
    pub operator: Op,
    pub case_sensitive: bool,
    pub regex: bool,
    pub negate: bool,
}

impl SearchTerm {
    pub fn new(term: &str, operator: Op) -> Self {
        Self {
            term: term.to_string(),
            operator,
            case_sensitive: false,
            regex: false,
            negate: false,
        }
    }
    pub fn case(mut self, v: bool) -> Self {
        self.case_sensitive = v;
        self
    }
    pub fn rx(mut self, v: bool) -> Self {
        self.regex = v;
        self
    }
    pub fn neg(mut self, v: bool) -> Self {
        self.negate = v;
        self
    }
}

/// Extra predicates that AND-compose with the stack (mirrors `composeWhere`).
#[derive(Clone, Default)]
pub struct Scope {
    /// Inclusive epoch-ms `[from, to]`; untimestamped lines are excluded when set.
    pub time_range: Option<(i64, i64)>,
    /// Restrict to these canonical level codes; empty = no constraint.
    pub levels: Vec<Level>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    pub matching_lines: Vec<usize>,
    pub total_matches: usize,
}

enum Matcher {
    CaseSensitive(String),
    CaseInsensitive(String),
    Rx(Regex),
}

struct CTerm {
    matcher: Matcher,
    negate: bool,
}

impl CTerm {
    fn is_match(&self, text: &str) -> bool {
        let base = match &self.matcher {
            Matcher::CaseSensitive(n) => text.contains(n.as_str()),
            Matcher::CaseInsensitive(n) => contains_ascii_ci(text, n),
            Matcher::Rx(r) => r.is_match(text),
        };
        base ^ self.negate
    }
}

fn compile_term(t: &SearchTerm) -> Result<CTerm, regex::Error> {
    let matcher = if t.regex {
        Matcher::Rx(
            RegexBuilder::new(&t.term)
                .case_insensitive(!t.case_sensitive)
                .build()?,
        )
    } else if t.case_sensitive {
        Matcher::CaseSensitive(t.term.clone())
    } else {
        Matcher::CaseInsensitive(t.term.clone())
    };
    Ok(CTerm {
        matcher,
        negate: t.negate,
    })
}

// Group the stack into AND-groups; a new group begins at the first term and at
// every `OR` term. The line matches if ANY group matches ALL its terms.
fn compile_groups(stack: &[SearchTerm]) -> Result<Vec<Vec<CTerm>>, regex::Error> {
    let mut groups: Vec<Vec<CTerm>> = Vec::new();
    for (i, t) in stack.iter().enumerate() {
        let ct = compile_term(t)?;
        if i == 0 || t.operator == Op::Or {
            groups.push(vec![ct]);
        } else {
            groups.last_mut().unwrap().push(ct);
        }
    }
    Ok(groups)
}

fn stack_matches(groups: &[Vec<CTerm>], text: &str) -> bool {
    groups.is_empty() || groups.iter().any(|g| g.iter().all(|c| c.is_match(text)))
}

fn scope_matches(index: &LineIndex, scope: &Scope, n: usize) -> bool {
    if let Some((from, to)) = scope.time_range {
        match index.ts_of(n) {
            Some(ts) if (from..=to).contains(&ts) => {}
            _ => return false,
        }
    }
    if !scope.levels.is_empty() && !scope.levels.contains(&index.level_of(n)) {
        return false;
    }
    true
}

/// Run a search over the index. Line numbers returned ascending (1-based).
pub fn search(
    index: &LineIndex,
    stack: &[SearchTerm],
    scope: &Scope,
) -> Result<SearchResult, regex::Error> {
    let groups = compile_groups(stack)?;
    let total = index.total_lines();
    // Parallel scan across cores. rayon's `collect` preserves iteration order, so
    // the result stays ascending without an explicit sort.
    let matching: Vec<usize> = (1..=total)
        .into_par_iter()
        .filter(|&n| scope_matches(index, scope, n) && stack_matches(&groups, &index.get_line(n)))
        .collect();
    Ok(SearchResult {
        total_matches: matching.len(),
        matching_lines: matching,
    })
}

/// Count lines matching a single term (mirrors `SearchEngine.countTerm`).
pub fn count_term(
    index: &LineIndex,
    term: &str,
    case_sensitive: bool,
    regex: bool,
) -> Result<usize, regex::Error> {
    if term.is_empty() {
        return Ok(0);
    }
    let stack = [SearchTerm::new(term, Op::And)
        .case(case_sensitive)
        .rx(regex)];
    Ok(search(index, &stack, &Scope::default())?.total_matches)
}

// ASCII case-insensitive substring test (matches SQLite's ASCII-only `lower()`).
fn contains_ascii_ci(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() {
        return true;
    }
    if n.len() > h.len() {
        return false;
    }
    (0..=h.len() - n.len())
        .any(|start| h[start..start + n.len()].iter().zip(n).all(|(a, b)| a.eq_ignore_ascii_case(b)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::index::{IndexOptions, LineIndex};
    use std::fs::File;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn open_lines(lines: &[&str]) -> LineIndex {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "zerch_search_{}_{}.log",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        File::create(&p)
            .unwrap()
            .write_all(lines.join("\n").as_bytes())
            .unwrap();
        let idx = LineIndex::open(&p, IndexOptions::default(), |_| {}).unwrap();
        let _ = std::fs::remove_file(&p);
        idx
    }

    fn lines(idx: &LineIndex, stack: &[SearchTerm]) -> Vec<usize> {
        search(idx, stack, &Scope::default()).unwrap().matching_lines
    }
    fn t(term: &str, op: Op) -> SearchTerm {
        SearchTerm::new(term, op)
    }

    fn dataset() -> LineIndex {
        open_lines(&[
            "ERROR something timeout", // 1
            "INFO all good",           // 2
            "WARN connection lost",    // 3
            "error lower timeout",     // 4 (lowercase error)
            "INFO connection ok",      // 5
        ])
    }

    #[test]
    fn empty_stack_returns_all() {
        let idx = dataset();
        let r = search(&idx, &[], &Scope::default()).unwrap();
        assert_eq!(r.total_matches, 5);
        assert_eq!(r.matching_lines, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn single_term_case_sensitivity() {
        let idx = dataset();
        assert_eq!(lines(&idx, &[t("error", Op::And)]), vec![1, 4]);
        assert_eq!(lines(&idx, &[t("ERROR", Op::And).case(true)]), vec![1]);
    }

    #[test]
    fn and_or() {
        let idx = dataset();
        assert_eq!(
            lines(&idx, &[t("error", Op::And), t("timeout", Op::And)]),
            vec![1, 4]
        );
        assert_eq!(
            lines(&idx, &[t("WARN", Op::And), t("INFO", Op::Or)]),
            vec![2, 3, 5]
        );
    }

    #[test]
    fn counts() {
        let idx = dataset();
        assert_eq!(count_term(&idx, "connection", false, false).unwrap(), 2);
        assert_eq!(count_term(&idx, "timeout", false, false).unwrap(), 2);
        assert_eq!(count_term(&idx, "nope", false, false).unwrap(), 0);
    }

    #[test]
    fn regex_matching() {
        let idx = open_lines(&[
            "GET /api/users 200 ok",      // 1
            "GET /api/users 404 missing", // 2
            "POST /api/login 500 boom",   // 3
            "POST /api/login 503 retry",  // 4
            "heartbeat ok",               // 5
        ]);
        assert_eq!(lines(&idx, &[t("5\\d\\d", Op::And).rx(true)]), vec![3, 4]);
        assert_eq!(lines(&idx, &[t("\\bget\\b", Op::And).rx(true)]), vec![1, 2]);
        assert_eq!(
            lines(&idx, &[t("\\bget\\b", Op::And).rx(true).case(true)]),
            Vec::<usize>::new()
        );
        assert_eq!(
            lines(&idx, &[t("POST", Op::And), t("50\\d", Op::And).rx(true)]),
            vec![3, 4]
        );
        assert_eq!(
            lines(&idx, &[t("404", Op::And), t("50\\d", Op::Or).rx(true)]),
            vec![2, 3, 4]
        );
        assert_eq!(count_term(&idx, "5\\d\\d", false, true).unwrap(), 2);
        assert_eq!(count_term(&idx, "/api/\\w+", false, true).unwrap(), 4);
        // literal term with regex chars, regex off → treated literally
        assert_eq!(lines(&idx, &[t("/api/users", Op::And)]), vec![1, 2]);
    }

    #[test]
    fn invalid_regex_is_err() {
        let idx = dataset();
        assert!(search(&idx, &[t("(", Op::And).rx(true)], &Scope::default()).is_err());
        assert!(count_term(&idx, "(", false, true).is_err());
    }

    #[test]
    fn negation() {
        let idx = dataset();
        assert_eq!(lines(&idx, &[t("error", Op::And).neg(true)]), vec![2, 3, 5]);
        assert_eq!(
            lines(
                &idx,
                &[t("timeout", Op::And), t("ERROR", Op::And).case(true).neg(true)]
            ),
            vec![4]
        );
        assert_eq!(
            lines(&idx, &[t("conn.*", Op::And).rx(true).neg(true)]),
            vec![1, 2, 4]
        );
        assert_eq!(
            lines(&idx, &[t("WARN", Op::And), t("timeout", Op::Or).neg(true)]),
            vec![2, 3, 5]
        );
    }

    fn timed() -> LineIndex {
        open_lines(&[
            "2026-06-20T00:00:00Z ERROR a", // 1
            "2026-06-20T01:00:00Z INFO b",  // 2
            "no timestamp ERROR c",         // 3
            "2026-06-20T02:00:00Z ERROR d", // 4
            "2026-06-20T03:00:00Z INFO e",  // 5
        ])
    }
    fn at(h: u32) -> i64 {
        use chrono::{TimeZone, Utc};
        Utc.with_ymd_and_hms(2026, 6, 20, h, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn time_scope() {
        let idx = timed();
        let scope = |from, to| Scope {
            time_range: Some((from, to)),
            ..Default::default()
        };
        assert_eq!(
            search(&idx, &[], &scope(at(0), at(2))).unwrap().matching_lines,
            vec![1, 2, 4] // line 3 has no timestamp
        );
        assert_eq!(
            search(&idx, &[t("ERROR", Op::And)], &scope(at(1), at(3)))
                .unwrap()
                .matching_lines,
            vec![4]
        );
        assert_eq!(
            search(&idx, &[], &Scope::default()).unwrap().matching_lines,
            vec![1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn level_scope() {
        let idx = open_lines(&[
            "INFO start",  // 1 level 3
            "ERROR boom",  // 2 level 5
            "plain line",  // 3 level 0
            "WARN careful", // 4 level 4
            "ERROR again", // 5 level 5
        ]);
        let scope = |levels: Vec<Level>| Scope {
            levels,
            ..Default::default()
        };
        assert_eq!(
            search(&idx, &[], &scope(vec![5])).unwrap().matching_lines,
            vec![2, 5]
        );
        assert_eq!(
            search(&idx, &[], &scope(vec![4, 5])).unwrap().matching_lines,
            vec![2, 4, 5]
        );
        assert_eq!(
            search(&idx, &[t("again", Op::And)], &scope(vec![5]))
                .unwrap()
                .matching_lines,
            vec![5]
        );
        assert_eq!(
            search(&idx, &[], &scope(vec![])).unwrap().matching_lines,
            vec![1, 2, 3, 4, 5]
        );
    }
}
