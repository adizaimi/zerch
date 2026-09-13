//! Stateless request handlers. Each takes the parsed query and returns a
//! `(status, json)` pair, so they unit-test without a socket. The displayed
//! sequence (the expensive `search` + merge) is memoised by view token.

use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::search::{search, Op, Scope, SearchTerm};
use crate::view::displayed::{build_displayed_items, DisplayedItem};
use crate::view::state::{parse_view, serialize_view, FilterMode, ViewState};

use super::render;
use super::State;

/// The on-screen row sequence for a view token. `All(n)` is the identity case
/// (no filters, no hidden lines): position `i` is line `i+1`, so opening a large
/// file with no filters costs nothing — we never materialise a per-line vector.
pub enum Displayed {
    All(usize),
    Items(Vec<DisplayedItem>),
}

impl Displayed {
    pub fn len(&self) -> usize {
        match self {
            Displayed::All(n) => *n,
            Displayed::Items(v) => v.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// The item at displayed position `i` (0-based), or None if out of range.
    pub fn at(&self, i: usize) -> Option<DisplayedItem> {
        match self {
            Displayed::All(n) => (i < *n).then(|| DisplayedItem::Line(i + 1)),
            Displayed::Items(v) => v.get(i).cloned(),
        }
    }
}

// Mirror the TUI's `bad_regex` guard: a filter whose regex won't compile is dropped
// from the stack so the *other* filters still apply (rather than failing the search).
fn bad_regex(term: &str) -> bool {
    regex::Regex::new(term).is_err()
}

/// Canonical cache key: only the fields that change the displayed sequence
/// (filters, mode, bookmarks, visible, hidden). Search, highlights, syntax, hide-cols,
/// selection and the cursor don't affect *which* rows show, so editing them must not
/// invalidate the cache and trigger a needless filter re-scan.
fn displayed_key(view: &ViewState) -> String {
    let reduced = ViewState {
        filters: view.filters.clone(),
        filter_mode: view.filter_mode,
        bookmarks: view.bookmarks.clone(),
        visible: view.visible.clone(),
        hidden: view.hidden.clone(),
        ..ViewState::default()
    };
    serialize_view(&reduced)
}

/// Compute (or fetch from cache) the displayed sequence for a view.
pub fn displayed_for(state: &State, _token: &str, view: &ViewState) -> Arc<Displayed> {
    let key = displayed_key(view);
    // Cache hit: move to front (LRU) and return.
    {
        let mut cache = state.cache.lock().unwrap();
        if let Some(pos) = cache.iter().position(|(k, _)| k == &key) {
            let entry = cache.remove(pos);
            let arc = Arc::clone(&entry.1);
            cache.insert(0, entry);
            return arc;
        }
    }

    let total = state.index.total_lines();
    let stack: Vec<SearchTerm> = build_stack(view);
    let displayed = if stack.is_empty() && view.hidden.is_empty() {
        // Unfiltered and nothing hidden → every line, in order, no gaps.
        Displayed::All(total)
    } else {
        let matching: Vec<usize> = if stack.is_empty() {
            (1..=total).collect() // unfiltered base; hidden lines carve gaps
        } else {
            search(&state.index, &stack, &Scope::default())
                .map(|r| r.matching_lines)
                .unwrap_or_default()
        };
        Displayed::Items(build_displayed_items(
            &matching,
            &view.bookmarks,
            &[],
            &view.visible,
            &view.hidden,
            total,
        ))
    };

    let arc = Arc::new(displayed);
    {
        let mut cache = state.cache.lock().unwrap();
        cache.insert(0, (key, Arc::clone(&arc)));
        cache.truncate(super::CACHE_CAP);
    }
    arc
}

// Enabled, non-empty, valid-regex filters mapped to the search stack, all sharing
// the global AND/OR operator (matches the TUI's `recompute`).
fn build_stack(view: &ViewState) -> Vec<SearchTerm> {
    let op = match view.filter_mode {
        FilterMode::And => Op::And,
        FilterMode::Or => Op::Or,
    };
    view.filters
        .iter()
        .filter(|f| f.enabled && !f.term.is_empty() && !(f.regex && bad_regex(&f.term)))
        .map(|f| {
            SearchTerm::new(&f.term, op)
                .case(f.case_sensitive)
                .rx(f.regex)
                .neg(f.negate)
        })
        .collect()
}

/// `GET /rows?view=&from=&to=` — render displayed positions `[from, to)` to HTML.
pub fn rows(state: &State, q: &HashMap<String, String>) -> (u16, String) {
    let token = q.get("view").cloned().unwrap_or_default();
    let view = parse_view(&token);
    let displayed = displayed_for(state, &token, &view);
    let total = displayed.len();

    let from = q.get("from").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0).min(total);
    let to = q.get("to").and_then(|s| s.parse::<usize>().ok()).unwrap_or(total).min(total);
    let to = to.max(from);
    // Transient client text selection (teal lane); never part of the view token.
    let sel = q.get("sel").map(String::as_str).filter(|s| !s.is_empty());

    let mut arr: Vec<serde_json::Value> = Vec::with_capacity(to - from);
    for i in from..to {
        match displayed.at(i) {
            Some(DisplayedItem::Line(nn)) => {
                let line = state.index.get_line(nn);
                arr.push(serde_json::json!({
                    "t": "l",
                    "n": nn,
                    "lvl": render::level_hex(state.index.level_of(nn)),
                    "html": render::line_html(&view, &line, sel),
                }));
            }
            Some(DisplayedItem::Gap { start, end }) => {
                arr.push(serde_json::json!({
                    "t": "g",
                    "start": start,
                    "end": end,
                    "count": end - start + 1,
                }));
            }
            None => break,
        }
    }

    let body = serde_json::json!({ "rows": arr, "total": total, "from": from });
    (200, body.to_string())
}

/// `GET /matches?view=` — displayed positions of lines matching `view.search`, for
/// the match counter, n/N navigation, and (later) the minimap. Ascending. Uses the
/// engine's parallel search, then maps matched line numbers to displayed positions.
pub fn matches(state: &State, q: &HashMap<String, String>) -> (u16, String) {
    let token = q.get("view").cloned().unwrap_or_default();
    let view = parse_view(&token);
    let displayed = displayed_for(state, &token, &view);

    let mut positions: Vec<usize> = Vec::new();
    if !view.search.is_empty() {
        let stack = [SearchTerm::new(&view.search, Op::And)
            .case(view.search_case)
            .rx(view.search_regex)];
        let matched = search(&state.index, &stack, &Scope::default())
            .map(|r| r.matching_lines)
            .unwrap_or_default();
        positions = match &*displayed {
            // Every line is shown, so line n sits at position n-1.
            Displayed::All(_) => matched.iter().map(|&n| n - 1).collect(),
            Displayed::Items(v) => {
                let mut line_to_pos: HashMap<usize, usize> = HashMap::new();
                for (i, it) in v.iter().enumerate() {
                    if let DisplayedItem::Line(n) = it {
                        line_to_pos.insert(*n, i);
                    }
                }
                let mut p: Vec<usize> =
                    matched.iter().filter_map(|n| line_to_pos.get(n).copied()).collect();
                p.sort_unstable();
                p
            }
        };
    }

    let body = serde_json::json!({ "positions": positions, "total": positions.len() });
    (200, body.to_string())
}

/// `GET /goto?view=&line=N` — the displayed position of raw line N, or 404 if it is
/// hidden / out of range.
pub fn goto(state: &State, q: &HashMap<String, String>) -> (u16, String) {
    let token = q.get("view").cloned().unwrap_or_default();
    let view = parse_view(&token);
    let displayed = displayed_for(state, &token, &view);

    let line = match q.get("line").and_then(|s| s.parse::<usize>().ok()) {
        Some(l) if l >= 1 => l,
        _ => return (400, "{}".to_string()),
    };
    let pos = match &*displayed {
        Displayed::All(n) => (line <= *n).then_some(line - 1),
        Displayed::Items(v) => v
            .iter()
            .position(|it| matches!(it, DisplayedItem::Line(x) if *x == line)),
    };
    match pos {
        Some(p) => (200, serde_json::json!({ "pos": p }).to_string()),
        None => (404, "{}".to_string()),
    }
}
