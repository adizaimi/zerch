//! The interactive application: all view state, key handling, movement, search,
//! highlighting, and rendering. Filters / bookmarks / gap-reveal and overlays
//! layer on in the following commits/phases.

use crate::engine::index::{Histogram, LineIndex};
use crate::tui::history::HISTORY_MAX;
use crate::engine::search::{search, Op, Scope, SearchTerm};
use crate::levels::{hex_rgb, level_rgb};
use crate::view::displayed::{build_displayed_items, DisplayedItem};
use crate::view::highlight::{
    build_spans, color_json_line, match_ranges, syntax_ranges, HighlightSpec, SyntaxTok,
};
use crate::view::jsonline::{format_json, parse_json_line};
use crate::view::shell::{build_shell_command, ShellCommandOptions, ShellFilter};
use crate::view::state::{serialize_view, FilterEntry, FilterMode, HighlightEntry, ViewState};
use crate::view::HIGHLIGHT_PALETTE;
use std::collections::{BTreeSet, HashMap};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

const HINT: &str =
    "j/k f/b H/M/L · /search n/N :N goto · F filter h hl B mark x hide · T hist m menu · w wrap ? help q quit";
const CURSOR_BG: Color = Color::Indexed(236);
// Faint zebra stripe on alternate logical lines; darker than CURSOR_BG so the
// cursor line still stands out over a striped row. Tuned for dark terminals.
const ZEBRA_BG: Color = Color::Indexed(234);
const EXPAND_CHUNK: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Search,
    Highlight,
    Filter,
    Hist,
    /// `:` command line (currently `:N` / `:$` to jump to a line).
    Command,
}

/// Where H/M/L place the cursor within the visible screen.
#[derive(Clone, Copy)]
enum ScreenPos {
    Top,
    Middle,
    Bottom,
}

/// The inline-editable fields in the menu (field-hiding config + bookmarks).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MenuField {
    Separator,
    Columns,
    Bookmarks,
}

/// How a line is split into columns for field-hiding.
enum FieldSep {
    Char(char),
    Whitespace,
}

/// Interpret the separator field text: `space`/`ws`/`whitespace` → whitespace
/// runs, `tab` → a tab, empty → the default comma, otherwise the first char.
fn parse_sep(s: &str) -> FieldSep {
    match s.trim() {
        "space" | "ws" | "whitespace" => FieldSep::Whitespace,
        "tab" => FieldSep::Char('\t'),
        "" => FieldSep::Char(','),
        other => FieldSep::Char(other.chars().next().unwrap_or(',')),
    }
}

/// Parse a "1,2,5" column list into sorted, deduped 1-based indices. Each
/// comma-separated token is either a single number or an inclusive range
/// "A-B" (expanded to A..=B). Reversed ranges (B < A) are ignored, and a
/// range is capped at RANGE_CAP entries so a typo like "1-999999" can't blow
/// up memory. Unparseable tokens are silently dropped.
fn parse_cols(s: &str) -> Vec<usize> {
    const RANGE_CAP: usize = 4096;
    let mut v: Vec<usize> = Vec::new();
    for tok in s.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        // Split on the first '-' to allow "A-B" ranges; a bare number has none.
        if let Some((a, b)) = tok.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                if a >= 1 && b >= a {
                    v.extend((a..=b).take(RANGE_CAP));
                }
            }
        } else if let Ok(n) = tok.parse::<usize>() {
            if n >= 1 {
                v.push(n);
            }
        }
    }
    v.sort_unstable();
    v.dedup();
    v
}

const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Per-term hit-count state. Distinguishes "not yet scanned" from "scanned but
/// unavailable" (empty term or bad regex) so the menu can render "…" for pending
/// vs "—" for failed, and the background scheduler knows when it's done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Count {
    Pending,
    Value(usize),
    Failed,
}

impl Count {
    fn from_option(o: Option<usize>) -> Self {
        match o {
            Some(n) => Self::Value(n),
            None => Self::Failed,
        }
    }
    fn is_pending(self) -> bool {
        matches!(self, Self::Pending)
    }
}

// A full-screen overlay above the viewport. Text overlays (view string / shell
// command) carry their own content; the rest render from a source line.
enum Overlay {
    None,
    Json(usize),
    Line(usize),
    Help,
    Text { title: String, body: Vec<String> },
    Menu,
}

pub struct App {
    index: LineIndex,
    file_name: String,
    total: usize,
    displayed: Option<Vec<DisplayedItem>>,
    // line number → position in `displayed`, rebuilt whenever `displayed` changes,
    // so pos_of_line / in_view are O(1) rather than a linear scan.
    line_to_pos: HashMap<usize, usize>,
    // Cached search matches that fall in the current view (rebuilt on search /
    // view change), so the status bar doesn't recompute it every frame.
    miv: Vec<usize>,

    pos: usize,
    cursor: usize,
    hscroll: usize,
    wrap: bool,

    width: u16,
    height: usize,

    mode: Mode,
    input: String,
    input_cursor: usize, // caret position within `input`, in chars
    // Pending regex/case flags for the term being typed (filter/highlight apply
    // these on Enter; ^R / ^A toggle them, mirroring search).
    input_regex: bool,
    input_case: bool,

    search_term: String,
    search_regex: bool,
    search_case: bool,
    search_error: Option<&'static str>,
    /// Transient status-bar message (e.g. "no timestamps"); cleared on next keypress.
    hint: Option<&'static str>,
    matches: Vec<usize>,
    match_idx: usize,
    // Shared recent-input history across search / filter / highlight (newest first).
    history: Vec<String>,
    hist_idx: i64, // -1 = not browsing

    highlights: Vec<HighlightEntry>,
    /// Menu-toggled per-line syntax colouring (logfmt / whole-line JSON).
    syntax_color: bool,
    /// Menu-toggled colouring of filter-matched terms in the rendered line (on by
    /// default). When off, filters still reduce the view but aren't boxed.
    color_filters: bool,
    /// Menu-toggled zebra striping: a faint background on alternate logical lines
    /// (like the web UI). Local display pref — not part of the shareable view token.
    zebra: bool,
    filters: Vec<FilterEntry>,
    filter_mode: FilterMode,
    bookmarks: BTreeSet<usize>,
    visible_lines: BTreeSet<usize>,
    hidden_lines: BTreeSet<usize>,
    time_range: Option<(i64, i64)>,
    show_hist: bool,
    hist: Option<Histogram>,
    hist_cursor: usize,
    hist_anchor: usize,

    overlay: Overlay,
    overlay_scroll: usize,
    overlay_avail: usize, // last-rendered overlay inner height
    menu_sel: usize,
    // Per-term hit counts, in lock-step with `filters` and `highlights`. New
    // entries start `Pending`; the run-loop's idle time drains them by scanning
    // one term per frame. Menu edits (regex/case/negate) reset the affected slot
    // to `Pending` so it recomputes shortly after.
    filter_counts: Vec<Count>,
    highlight_counts: Vec<Count>,
    /// Numeric prefix being typed in normal mode (e.g. `12` before `→`); consumed
    /// as the pan distance in columns. `None` when no digits are pending.
    pending_count: Option<usize>,

    /// Field-hiding: split each line on `col_sep_text` and replace the 1-based
    /// columns listed in `hide_cols_text` with `…`. Both are edited inline in the
    /// menu. `menu_edit` is the field currently being edited (into `self.input`).
    col_sep_text: String,
    hide_cols_text: String,
    menu_edit: Option<MenuField>,

    confirm_quit: bool,
    pub should_quit: bool,
    /// Set on Ctrl-Z; the run loop suspends the process to the background.
    pub suspend: bool,
}

impl App {
    pub fn new(index: LineIndex, file_name: String) -> Self {
        let total = index.total_lines();
        App {
            index,
            file_name,
            total,
            displayed: None,
            line_to_pos: HashMap::new(),
            miv: Vec::new(),
            pos: 0,
            cursor: 0,
            hscroll: 0,
            wrap: false,
            width: 80,
            height: 24,
            mode: Mode::Normal,
            input: String::new(),
            input_cursor: 0,
            input_regex: false,
            input_case: false,
            search_term: String::new(),
            search_regex: false,
            search_case: false,
            search_error: None,
            hint: None,
            matches: Vec::new(),
            match_idx: 0,
            history: Vec::new(),
            hist_idx: -1,
            highlights: Vec::new(),
            syntax_color: false,
            color_filters: true,
            zebra: false,
            filters: Vec::new(),
            filter_mode: FilterMode::And,
            bookmarks: BTreeSet::new(),
            visible_lines: BTreeSet::new(),
            hidden_lines: BTreeSet::new(),
            time_range: None,
            show_hist: false,
            hist: None,
            hist_cursor: 0,
            hist_anchor: 0,
            overlay: Overlay::None,
            overlay_scroll: 0,
            overlay_avail: 0,
            menu_sel: 0,
            filter_counts: Vec::new(),
            highlight_counts: Vec::new(),
            pending_count: None,
            col_sep_text: ",".to_string(),
            hide_cols_text: String::new(),
            menu_edit: None,
            confirm_quit: false,
            should_quit: false,
            suspend: false,
        }
    }

    /// Restore a saved view (filters/highlights/bookmarks/revealed ranges/search
    /// and the selected line, revealing it if it's filtered out).
    pub fn apply_view(&mut self, v: ViewState) {
        self.filter_mode = v.filter_mode;
        self.filters = v.filters;
        self.highlights = v.highlights;
        // Counts are marked pending; the run-loop scheduler fills them in
        // between frames so the viewport shows immediately.
        self.reset_counts_pending();
        self.bookmarks = v.bookmarks.into_iter().collect();
        self.visible_lines = v.visible.into_iter().collect();
        self.hidden_lines = v.hidden.into_iter().collect();
        self.hide_cols_text = v.hide_cols;
        self.col_sep_text = if v.col_sep.is_empty() { ",".to_string() } else { v.col_sep };
        self.syntax_color = v.syntax;
        self.search_regex = v.search_regex;
        self.search_case = v.search_case;
        if !v.search.is_empty() {
            self.search_term = v.search.clone();
            if self.search_regex && bad_regex(&self.search_term) {
                self.search_error = Some("bad regex");
            } else if let Ok(res) = search(
                &self.index,
                &[SearchTerm::new(&self.search_term, Op::And)
                    .case(self.search_case)
                    .rx(self.search_regex)],
                &Scope::default(),
            ) {
                self.matches = res.matching_lines;
            }
        }
        self.recompute();
        if let Some(sel) = v.selected {
            if let Some(p) = self.pos_of_line(sel) {
                self.jump_to(p as i64);
            } else if sel >= 1 && sel <= self.total {
                self.visible_lines.insert(sel);
                self.recompute();
                if let Some(p) = self.pos_of_line(sel) {
                    self.jump_to(p as i64);
                }
            }
        }
    }

    /// Seed the shared recent-input history (search / filter / highlight) from
    /// persisted storage. Expects newest-first order; capped at [`HISTORY_MAX`].
    pub fn set_history(&mut self, mut h: Vec<String>) {
        h.truncate(HISTORY_MAX);
        self.history = h;
    }

    /// A copy of the current recent-input history (newest first) for persisting.
    pub fn history_snapshot(&self) -> Vec<String> {
        self.history.clone()
    }

    /// The current view serialised as a `--view` string (for session save).
    pub fn view_string(&self) -> String {
        serialize_view(&self.current_view())
    }

    fn count(&self) -> usize {
        self.displayed.as_ref().map_or(self.total, |d| d.len())
    }

    fn item_at(&self, pos: usize) -> DisplayedItem {
        match &self.displayed {
            Some(d) => d.get(pos).cloned().unwrap_or(DisplayedItem::Line(0)),
            None => DisplayedItem::Line(pos + 1),
        }
    }

    fn in_view(&self, ln: usize) -> bool {
        self.pos_of_line(ln).is_some()
    }

    fn pos_of_line(&self, ln: usize) -> Option<usize> {
        match &self.displayed {
            Some(_) => self.line_to_pos.get(&ln).copied(),
            None => (ln >= 1 && ln <= self.total).then(|| ln - 1),
        }
    }

    // Rebuild the line→position index and the cached in-view match list. Called
    // whenever `displayed` or `matches` change (never per render frame).
    fn rebuild_line_index(&mut self) {
        self.line_to_pos.clear();
        if let Some(d) = &self.displayed {
            for (i, it) in d.iter().enumerate() {
                if let DisplayedItem::Line(n) = it {
                    self.line_to_pos.insert(*n, i);
                }
            }
        }
    }

    fn refresh_matches_in_view(&mut self) {
        self.miv = if self.search_term.is_empty() {
            Vec::new()
        } else {
            self.matches.iter().copied().filter(|&l| self.in_view(l)).collect()
        };
        if self.match_idx >= self.miv.len() {
            self.match_idx = 0;
        }
    }

    fn clamp_pos(&self, p: i64) -> usize {
        let max = self.count().saturating_sub(1) as i64;
        p.clamp(0, max.max(0)) as usize
    }

    // --- movement -------------------------------------------------------------

    fn move_cursor(&mut self, delta: i64) {
        let count = self.count();
        if count == 0 {
            return;
        }
        let nc = (self.cursor as i64 + delta).clamp(0, count as i64 - 1) as usize;
        self.cursor = nc;
        if self.wrap {
            self.scroll_wrapped_to_cursor();
            return;
        }
        let vp = self.height.max(1);
        let mut np = self.pos as i64;
        if (nc as i64) < np {
            np = nc as i64;
        } else if nc >= self.pos + vp {
            np = nc as i64 - vp as i64 + 1;
        }
        self.pos = self.clamp_pos(np);
    }

    fn page_by(&mut self, pages: i64) {
        let count = self.count();
        if count == 0 {
            return;
        }
        let vp = self.height.max(1) as i64;
        let delta = pages * vp;
        if self.wrap {
            let np = self.clamp_pos(self.pos as i64 + delta);
            self.pos = np;
            self.cursor = np;
            return;
        }
        let new_pos = self.clamp_pos(self.pos as i64 + delta);
        let a = new_pos as i64 + vp - 1;
        let b = (self.cursor as i64 + delta).max(new_pos as i64);
        let nc = (count as i64 - 1).min(a).min(b).max(0);
        self.pos = new_pos;
        self.cursor = nc as usize;
    }

    fn jump_to(&mut self, position: i64) {
        let count = self.count();
        if count == 0 {
            return;
        }
        let c = position.clamp(0, count as i64 - 1) as usize;
        self.cursor = c;
        self.pos = if self.wrap {
            self.clamp_pos(c as i64)
        } else {
            self.clamp_pos(c as i64 - self.height as i64 / 2)
        };
    }

    // Index of the last displayed item that is (at least partly) on screen,
    // given the current top (`pos`) and viewport height. Wrap-aware: in wrap
    // mode a single item can span several rows, so fewer items are visible.
    fn last_visible_pos(&self) -> usize {
        let count = self.count();
        if count == 0 {
            return 0;
        }
        let vp = self.height.max(1);
        if !self.wrap {
            return (self.pos + vp - 1).min(count - 1);
        }
        let mut rows = 0usize;
        let mut p = self.pos;
        while p < count {
            rows += self.wrap_rows_at(p);
            if rows >= vp {
                break;
            }
            p += 1;
        }
        p.min(count - 1)
    }

    // vim H/M/L: move the cursor within the visible screen without scrolling.
    fn cursor_to_screen(&mut self, where_: ScreenPos) {
        if self.count() == 0 {
            return;
        }
        let top = self.pos;
        let bottom = self.last_visible_pos();
        self.cursor = match where_ {
            ScreenPos::Top => top,
            ScreenPos::Middle => (top + bottom) / 2,
            ScreenPos::Bottom => bottom,
        };
    }

    // `:N` / `:$` — jump to a 1-based document line, revealing it if a filter
    // has hidden it (mirrors --view's `selected` restore).
    fn goto_line(&mut self, line: usize) {
        if self.total == 0 {
            return;
        }
        let n = line.clamp(1, self.total);
        if let Some(p) = self.pos_of_line(n) {
            self.jump_to(p as i64);
            return;
        }
        self.visible_lines.insert(n);
        self.recompute();
        if let Some(p) = self.pos_of_line(n) {
            self.jump_to(p as i64);
        }
    }

    fn run_command(&mut self, cmd: &str) {
        self.mode = Mode::Normal;
        let c = cmd.trim();
        if c.is_empty() {
            return;
        }
        if c == "q" || c == "quit" {
            self.should_quit = true; // vim :q — quit straight away (read-only viewer)
        } else if c == "$" {
            self.goto_line(self.total);
        } else if let Ok(n) = c.parse::<usize>() {
            self.goto_line(n);
        } else {
            self.hint = Some("unknown command (try :N, :$, or :q)");
        }
    }

    // Screen rows a displayed item occupies in wrap mode.
    fn wrap_rows_at(&self, p: usize) -> usize {
        match self.item_at(p) {
            DisplayedItem::Gap { .. } => 1,
            DisplayedItem::Line(n) => {
                let len = self.index.get_line(n).replace('\t', "  ").chars().count();
                let w = self.content_width();
                if len == 0 {
                    1
                } else {
                    len.div_ceil(w)
                }
            }
        }
    }

    // In wrap mode, scroll the top position minimally so the cursor's (possibly
    // multi-row) line stays fully visible — height-aware, like the non-wrap path.
    fn scroll_wrapped_to_cursor(&mut self) {
        if self.cursor < self.pos {
            self.pos = self.cursor;
            return;
        }
        let vp = self.height.max(1);
        let mut p = self.pos;
        let mut total: usize = (self.pos..=self.cursor).map(|q| self.wrap_rows_at(q)).sum();
        while total > vp && p < self.cursor {
            total -= self.wrap_rows_at(p);
            p += 1;
        }
        self.pos = p;
    }

    // After toggling wrap, reposition `pos` so the cursor stays centered and
    // visible. This prevents the cursor line from suddenly appearing in a
    // different screen position due to changed line heights.
    fn adjust_pos_for_wrap_toggle(&mut self) {
        if self.cursor == 0 {
            self.pos = 0;
            return;
        }
        let count = self.count();
        if count == 0 {
            return;
        }
        let vp = self.height.max(1);
        let target_row = vp / 2; // target screen row for the cursor
        let mut p = self.cursor;
        let mut rows_below = 0usize;
        // Walk backwards from cursor, summing screen rows, until we've
        // seen enough rows to place cursor at target_row.
        while rows_below < target_row && p > 0 {
            p -= 1;
            rows_below += self.wrap_rows_at(p);
        }
        self.pos = p;
    }

    // Pan horizontally by `count` columns, or by half a screen (min 8) when no
    // numeric prefix was given.
    fn pan(&mut self, dir: i64, count: Option<usize>) {
        if self.wrap {
            return;
        }
        let step = count.unwrap_or((self.width as usize / 2).max(8));
        let max_h = self.index.max_line_length().saturating_sub(2);
        self.hscroll = if dir < 0 {
            self.hscroll.saturating_sub(step)
        } else {
            (self.hscroll + step).min(max_h)
        };
    }

    fn content_width(&self) -> usize {
        (self.width as usize)
            .saturating_sub(self.gutter_width() + 1)
            .max(1)
    }

    // --- search ---------------------------------------------------------------

    fn run_search(&mut self, term: &str) {
        self.mode = Mode::Normal;
        if term.is_empty() {
            return;
        }
        push_history(&mut self.history, term);
        self.search_term = term.to_string();
        self.match_idx = 0;
        if self.search_regex && bad_regex(term) {
            self.search_error = Some("bad regex");
            self.matches.clear();
            self.refresh_matches_in_view();
            return;
        }
        self.search_error = None;
        let stack = [SearchTerm::new(term, Op::And)
            .case(self.search_case)
            .rx(self.search_regex)];
        match search(&self.index, &stack, &Scope::default()) {
            Ok(res) => self.matches = res.matching_lines,
            Err(_) => {
                self.matches.clear();
                self.search_error = Some("error");
            }
        }
        self.refresh_matches_in_view();
        if let Some(&first) = self.miv.first() {
            if let Some(p) = self.pos_of_line(first) {
                self.jump_to(p as i64);
            }
            self.scroll_to_col(first);
        }
    }

    // n / N: jump to the next/previous match relative to the *cursor* (vim
    // style), so match navigation tracks wherever you've moved to. `miv` is
    // sorted ascending. Wraps around the ends.
    fn goto_match_relative(&mut self, forward: bool) {
        if self.miv.is_empty() {
            return;
        }
        let cur = self.current_line().unwrap_or(0);
        let idx = if forward {
            self.miv.iter().position(|&l| l > cur).unwrap_or(0)
        } else {
            self.miv.iter().rposition(|&l| l < cur).unwrap_or(self.miv.len() - 1)
        };
        self.match_idx = idx;
        let line = self.miv[idx];
        if let Some(p) = self.pos_of_line(line) {
            self.jump_to(p as i64);
        }
        self.scroll_to_col(line);
    }

    // Scroll horizontally so the first match on `line` is on screen (~8 cols in).
    fn scroll_to_col(&mut self, line: usize) {
        if self.wrap {
            return;
        }
        let text = self.index.get_line(line).replace('\t', "  ");
        let ranges = match_ranges(&text, &self.search_term, self.search_case, self.search_regex);
        let Some(&(col, _)) = ranges.first() else {
            return;
        };
        let cw = self.content_width();
        if col < self.hscroll || col >= self.hscroll + cw {
            self.hscroll = col.saturating_sub(8);
        }
    }

    fn add_highlight(&mut self, term: &str) {
        if term.is_empty() {
            return;
        }
        push_history(&mut self.history, term);
        if self.highlights.iter().any(|h| h.term == term) {
            self.hint = Some("highlight already exists");
            return;
        }
        let color = HIGHLIGHT_PALETTE[self.highlights.len() % HIGHLIGHT_PALETTE.len()].to_string();
        self.highlights.push(HighlightEntry {
            term: term.to_string(),
            color,
            enabled: true,
            case_sensitive: self.input_case,
            regex: self.input_regex,
        });
        self.highlight_counts.push(Count::Pending);
    }

    // --- filters / bookmarks / gaps -------------------------------------------

    fn current_line(&self) -> Option<usize> {
        match self.item_at(self.cursor) {
            DisplayedItem::Line(n) => Some(n),
            DisplayedItem::Gap { .. } => None,
        }
    }

    // Rebuild the displayed sequence from the active filters + forced-visible lines.
    // `displayed` is `None` (show everything) unless a filter or time range is active.
    fn recompute(&mut self) {
        let op = match self.filter_mode {
            FilterMode::And => Op::And,
            FilterMode::Or => Op::Or,
        };
        let enabled: Vec<SearchTerm> = self
            .filters
            .iter()
            .filter(|f| f.enabled && !f.term.is_empty() && !(f.regex && bad_regex(&f.term)))
            .map(|f| {
                SearchTerm::new(&f.term, op)
                    .case(f.case_sensitive)
                    .rx(f.regex)
                    .neg(f.negate)
            })
            .collect();

        let unfiltered = enabled.is_empty() && self.time_range.is_none();
        if unfiltered && self.hidden_lines.is_empty() {
            self.displayed = None;
        } else {
            // Without a filter but with hidden lines, the base set is every line
            // (build_displayed_items subtracts `hidden`, leaving gaps where they were).
            let matches: Vec<usize> = if unfiltered {
                (1..=self.total).collect()
            } else {
                let scope = Scope {
                    time_range: self.time_range,
                    levels: Vec::new(),
                };
                search(&self.index, &enabled, &scope)
                    .map(|r| r.matching_lines)
                    .unwrap_or_default()
            };
            let bm: Vec<usize> = self.bookmarks.iter().copied().collect();
            let vis: Vec<usize> = self.visible_lines.iter().copied().collect();
            let hid: Vec<usize> = self.hidden_lines.iter().copied().collect();
            self.displayed = Some(build_displayed_items(&matches, &bm, &[], &vis, &hid, self.total));
        }
        self.rebuild_line_index();
        self.refresh_matches_in_view();
    }

    // Recompute, then keep the cursor on the same source line when possible.
    fn refresh_preserving(&mut self) {
        let ln = self.current_line();
        self.recompute();
        match ln.and_then(|l| self.pos_of_line(l)) {
            Some(p) => self.jump_to(p as i64),
            None => {
                self.cursor = self.cursor.min(self.count().saturating_sub(1));
                self.pos = self.clamp_pos(self.pos as i64);
            }
        }
    }

    fn add_filter(&mut self, term: &str) {
        if term.is_empty() {
            return;
        }
        push_history(&mut self.history, term);
        if self.filters.iter().any(|f| f.term == term) {
            self.hint = Some("filter already exists");
            return;
        }
        self.filters.push(FilterEntry {
            term: term.to_string(),
            enabled: true,
            case_sensitive: self.input_case,
            regex: self.input_regex,
            negate: false,
        });
        self.filter_counts.push(Count::Pending);
        self.recompute();
        self.pos = 0;
        self.cursor = 0;
    }

    fn toggle_filter_mode(&mut self) {
        self.filter_mode = match self.filter_mode {
            FilterMode::And => FilterMode::Or,
            FilterMode::Or => FilterMode::And,
        };
        self.refresh_preserving();
    }

    fn toggle_bookmark(&mut self) {
        if let DisplayedItem::Line(n) = self.item_at(self.cursor) {
            if !self.bookmarks.insert(n) {
                self.bookmarks.remove(&n);
            }
            // Bookmarks are forced-visible only within a filtered view.
            if self.displayed.is_some() {
                self.refresh_preserving();
            }
        }
    }

    // `x`: hide the cursor's line. It collapses into a gap; re-reveal it with the
    // usual gap keys (+/Enter/-) or "clear hidden" in the menu.
    fn hide_current_line(&mut self) {
        if let Some(n) = self.current_line() {
            self.hidden_lines.insert(n);
            self.refresh_preserving();
        }
    }

    fn goto_bookmark(&mut self, forward: bool) {
        if self.bookmarks.is_empty() {
            return;
        }
        let cur = self.current_line().unwrap_or(0);
        let sorted: Vec<usize> = self.bookmarks.iter().copied().collect();
        let target = if forward {
            sorted.iter().find(|&&b| b > cur).copied().or_else(|| sorted.first().copied())
        } else {
            sorted.iter().rev().find(|&&b| b < cur).copied().or_else(|| sorted.last().copied())
        };
        if let Some(t) = target {
            if let Some(p) = self.pos_of_line(t) {
                self.jump_to(p as i64);
            }
        }
    }

    fn gap_action(&mut self, from_top: bool) {
        if let DisplayedItem::Gap { start, end } = self.item_at(self.cursor) {
            let (lo, hi) = if from_top {
                (start, (start + EXPAND_CHUNK - 1).min(end))
            } else {
                (end.saturating_sub(EXPAND_CHUNK - 1).max(start), end)
            };
            for i in lo..=hi {
                self.visible_lines.insert(i);
                self.hidden_lines.remove(&i); // revealing a gap also un-hides
            }
            self.recompute();
            let focus = if from_top { lo } else { hi };
            match self.pos_of_line(focus) {
                Some(p) => self.jump_to(p as i64),
                None => {
                    self.cursor = self.cursor.min(self.count().saturating_sub(1));
                    self.pos = self.clamp_pos(self.pos as i64);
                }
            }
        }
    }

    // --- overlays -------------------------------------------------------------

    fn enter_action(&mut self) {
        match self.item_at(self.cursor) {
            DisplayedItem::Gap { .. } => self.gap_action(true),
            DisplayedItem::Line(n) => self.open_line(n),
        }
    }

    // Enter on a line: JSON → JSON view; too-long-to-fit → wrapped view; else no-op.
    fn open_line(&mut self, n: usize) {
        let content = self.index.get_line(n);
        if parse_json_line(&content).is_some() {
            self.overlay = Overlay::Json(n);
            self.overlay_scroll = 0;
            return;
        }
        let clean = content.replace('\t', "  ");
        if clean.chars().count() > self.content_width() {
            self.overlay = Overlay::Line(n);
            self.overlay_scroll = 0;
        }
    }

    fn handle_scroll_overlay_key(&mut self, key: KeyEvent) {
        let avail = self.overlay_avail.max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => self.overlay = Overlay::None,
            KeyCode::Char('j') | KeyCode::Down => self.overlay_scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => {
                self.overlay_scroll = self.overlay_scroll.saturating_sub(1)
            }
            KeyCode::Char(' ') | KeyCode::PageDown => self.overlay_scroll += avail,
            KeyCode::PageUp => self.overlay_scroll = self.overlay_scroll.saturating_sub(avail),
            KeyCode::Char('g') => self.overlay_scroll = 0,
            KeyCode::Char('G') => self.overlay_scroll = usize::MAX / 2, // clamped in render
            _ => {}
        }
    }

    // Per-character styled JSON (syntax base + highlight/search overlay).
    fn json_styled(&self, line: &str) -> Vec<(char, Style)> {
        let specs = self.enabled_highlight_specs();
        let spans = color_json_line(
            line,
            &specs,
            &self.search_term,
            self.search_case,
            self.search_regex,
        );
        let mut out = Vec::new();
        for s in spans {
            let style = if s.search {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else if s.hl {
                // highlight match: coloured box, one colour per term
                match &s.color {
                    Some(c) if c.starts_with('#') => {
                        let (r, g, b) = hex_rgb(c);
                        Style::default().fg(Color::Black).bg(Color::Rgb(r, g, b))
                    }
                    _ => Style::default().add_modifier(Modifier::BOLD),
                }
            } else {
                // JSON syntax base colour
                match &s.color {
                    Some(c) => Style::default().fg(name_color(c)),
                    None => Style::default(),
                }
            };
            for ch in s.text.chars() {
                out.push((ch, style));
            }
        }
        out
    }

    fn overlay_lines(&self, inner_w: usize) -> Vec<Line<'static>> {
        match &self.overlay {
            Overlay::Json(n) => {
                let content = self.index.get_line(*n);
                let mut lines = Vec::new();
                if let Some(jl) = parse_json_line(&content) {
                    if !jl.prefix.is_empty() {
                        lines.push(Line::from(Span::styled(
                            jl.prefix.clone(),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                    for pl in format_json(&jl.value).lines() {
                        lines.extend(wrap_styled(&self.json_styled(pl), inner_w));
                    }
                }
                lines
            }
            Overlay::Line(n) => wrap_styled(&self.style_line(*n, false), inner_w),
            Overlay::Text { body, .. } => {
                body.iter().flat_map(|l| wrap_plain(l, inner_w)).collect()
            }
            Overlay::Help => help_lines(),
            Overlay::None | Overlay::Menu => Vec::new(),
        }
    }

    fn overlay_title(&self) -> String {
        match &self.overlay {
            Overlay::Json(n) => format!(" JSON · line {n} · Esc/q close "),
            Overlay::Line(n) => format!(" line {n} · Esc/q close "),
            Overlay::Text { title, .. } => format!(" {title} · Esc/q close "),
            Overlay::Help => " Keys · Esc/q close ".to_string(),
            Overlay::None | Overlay::Menu => String::new(),
        }
    }

    fn render_overlay(&mut self, f: &mut Frame) {
        let area = f.area();
        let block = Block::bordered().title(self.overlay_title());
        let inner = block.inner(area);
        self.overlay_avail = inner.height as usize;
        let lines = self.overlay_lines(inner.width as usize);
        let max = lines.len().saturating_sub(inner.height as usize);
        self.overlay_scroll = self.overlay_scroll.min(max);
        let visible: Vec<Line> = lines
            .into_iter()
            .skip(self.overlay_scroll)
            .take(inner.height as usize)
            .collect();
        f.render_widget(Paragraph::new(visible).block(block), area);
    }

    // --- histogram / time brush ----------------------------------------------

    fn toggle_hist(&mut self) {
        if self.show_hist {
            // Preserve the current cursor line: only the displayed sequence needs
            // to be recomputed (in case a time range was active), not the view.
            self.show_hist = false;
            self.hist = None;
            self.time_range = None;
            self.mode = Mode::Normal;
            self.refresh_preserving();
        } else if self.index.time_bounds().is_none() {
            self.hint = Some("no timestamps in this file — histogram unavailable");
        } else {
            self.show_hist = true;
            self.hist_cursor = 0;
            self.hist_anchor = 0;
            self.mode = Mode::Hist;
        }
    }

    fn ensure_hist(&mut self, buckets: usize) {
        if self.hist.as_ref().map(|h| h.counts.len()) != Some(buckets) {
            self.hist = self.index.histogram(buckets);
        }
        if let Some(h) = &self.hist {
            let m = h.counts.len().saturating_sub(1);
            self.hist_cursor = self.hist_cursor.min(m);
            self.hist_anchor = self.hist_anchor.min(m);
        }
    }

    fn apply_time_brush(&mut self) {
        if let Some(h) = &self.hist {
            let lo = self.hist_anchor.min(self.hist_cursor) as i64;
            let hi = self.hist_anchor.max(self.hist_cursor) as i64;
            self.time_range = Some((
                h.min_ts + lo * h.bucket_ms,
                h.min_ts + (hi + 1) * h.bucket_ms - 1,
            ));
        }
        self.recompute();
        self.pos = 0;
        self.cursor = 0;
        self.mode = Mode::Normal;
    }

    fn handle_hist_key(&mut self, key: KeyEvent) {
        let n = self.hist.as_ref().map(|h| h.counts.len()).unwrap_or(0);
        match key.code {
            KeyCode::Char('h') | KeyCode::Left => {
                self.hist_cursor = self.hist_cursor.saturating_sub(1)
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if n > 0 {
                    self.hist_cursor = (self.hist_cursor + 1).min(n - 1);
                }
            }
            KeyCode::Char('a') => self.hist_anchor = self.hist_cursor,
            KeyCode::Char('c') => {
                self.time_range = None;
                self.recompute();
                self.pos = 0;
                self.cursor = 0;
                self.mode = Mode::Normal;
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.apply_time_brush(),
            KeyCode::Char('T') => self.toggle_hist(),
            // q or Esc → leave the brush; the histogram row stays until T.
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            _ => {}
        }
    }

    fn hist_selection(&self) -> (Option<usize>, Option<usize>) {
        if self.mode == Mode::Hist {
            return (
                Some(self.hist_anchor.min(self.hist_cursor)),
                Some(self.hist_anchor.max(self.hist_cursor)),
            );
        }
        if let (Some(h), Some((from, to))) = (&self.hist, self.time_range) {
            let m = h.counts.len().saturating_sub(1);
            let lo = (((from - h.min_ts) / h.bucket_ms).max(0) as usize).min(m);
            let hi = (((to - h.min_ts) / h.bucket_ms).max(0) as usize).min(m);
            return (Some(lo), Some(hi));
        }
        (None, None)
    }

    fn hist_line(&self) -> Line<'static> {
        let Some(h) = &self.hist else {
            return Line::from(String::new());
        };
        let max = h.counts.iter().copied().max().unwrap_or(1).max(1);
        let (lo, hi) = self.hist_selection();
        let mut spans = Vec::with_capacity(h.counts.len());
        for (i, &c) in h.counts.iter().enumerate() {
            let level_h = if c == 0 {
                0
            } else {
                (((c as f64 / max as f64) * 8.0).round() as usize).clamp(1, 8)
            };
            let mut st = match level_rgb(h.max_level[i]) {
                Some((r, g, b)) => Style::default().fg(Color::Rgb(r, g, b)),
                None => Style::default().fg(Color::DarkGray),
            };
            if let (Some(a), Some(b)) = (lo, hi) {
                if i >= a && i <= b {
                    st = st.add_modifier(Modifier::REVERSED);
                }
            }
            if self.mode == Mode::Hist && i == self.hist_cursor {
                st = st.add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(BLOCKS[level_h].to_string(), st));
        }
        Line::from(spans)
    }

    // --- menu -----------------------------------------------------------------

    fn sel_filter(&self) -> Option<usize> {
        (self.menu_sel < self.filters.len()).then_some(self.menu_sel)
    }
    fn sel_highlight(&self) -> Option<usize> {
        let nf = self.filters.len();
        (self.menu_sel >= nf && self.menu_sel < nf + self.highlights.len()).then(|| self.menu_sel - nf)
    }
    // The two inline-editable field rows sit just after the highlights.
    fn sel_field(&self) -> Option<MenuField> {
        let base = self.filters.len() + self.highlights.len();
        match self.menu_sel.checked_sub(base) {
            Some(0) => Some(MenuField::Separator),
            Some(1) => Some(MenuField::Columns),
            Some(2) => Some(MenuField::Bookmarks),
            _ => None,
        }
    }
    fn menu_rows(&self) -> usize {
        self.filters.len() + self.highlights.len() + 3 // + Separator, Hide Columns, Bookmarks
    }

    fn handle_menu_key(&mut self, key: KeyEvent) {
        // While editing an inline field, all keys go to the field editor.
        if self.menu_edit.is_some() {
            self.handle_menu_edit_key(key);
            return;
        }
        let total = self.menu_rows();
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('m') | KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Enter => {
                if let Some(field) = self.sel_field() {
                    self.menu_edit = Some(field);
                    self.input = match field {
                        MenuField::Separator => self.col_sep_text.clone(),
                        MenuField::Columns => self.hide_cols_text.clone(),
                        MenuField::Bookmarks => {
                            self.bookmarks.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(",")
                        }
                    };
                    self.input_cursor = self.input.chars().count();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.menu_sel = (self.menu_sel + 1).min(total - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => self.menu_sel = self.menu_sel.saturating_sub(1),
            KeyCode::Char(' ') => {
                if let Some(i) = self.sel_filter() {
                    self.filters[i].enabled = !self.filters[i].enabled;
                    self.refresh_preserving();
                } else if let Some(j) = self.sel_highlight() {
                    self.highlights[j].enabled = !self.highlights[j].enabled;
                }
            }
            KeyCode::Char('r') => {
                if let Some(i) = self.sel_filter() {
                    self.filters[i].regex = !self.filters[i].regex;
                    self.filter_counts[i] = Count::Pending;
                    self.refresh_preserving();
                } else if let Some(j) = self.sel_highlight() {
                    self.highlights[j].regex = !self.highlights[j].regex;
                    self.highlight_counts[j] = Count::Pending;
                }
            }
            KeyCode::Char('i') => {
                if let Some(i) = self.sel_filter() {
                    self.filters[i].case_sensitive = !self.filters[i].case_sensitive;
                    self.filter_counts[i] = Count::Pending;
                    self.refresh_preserving();
                } else if let Some(j) = self.sel_highlight() {
                    self.highlights[j].case_sensitive = !self.highlights[j].case_sensitive;
                    self.highlight_counts[j] = Count::Pending;
                }
            }
            KeyCode::Char('x') => {
                if let Some(i) = self.sel_filter() {
                    self.filters[i].negate = !self.filters[i].negate;
                    self.filter_counts[i] = Count::Pending;
                    self.refresh_preserving();
                }
            }
            KeyCode::Char('d') => {
                if let Some(i) = self.sel_filter() {
                    self.filters.remove(i);
                    self.filter_counts.remove(i);
                    self.refresh_preserving();
                } else if let Some(j) = self.sel_highlight() {
                    self.highlights.remove(j);
                    self.highlight_counts.remove(j);
                }
                let total = self.filters.len() + self.highlights.len();
                self.menu_sel = self.menu_sel.min(total.saturating_sub(1));
            }
            KeyCode::Char('o') => self.toggle_filter_mode(),
            KeyCode::Char('C') => {
                self.filters.clear();
                self.filter_counts.clear();
                self.recompute();
                self.pos = 0;
                self.cursor = 0;
                self.menu_sel = 0;
            }
            KeyCode::Char('c') => {
                self.highlights.clear();
                self.highlight_counts.clear();
                self.menu_sel = self.menu_sel.min(self.filters.len().saturating_sub(1));
            }
            KeyCode::Char('B') => {
                self.bookmarks.clear();
                if self.displayed.is_some() {
                    self.refresh_preserving();
                }
            }
            KeyCode::Char('X') => {
                if !self.hidden_lines.is_empty() {
                    self.hidden_lines.clear();
                    self.refresh_preserving();
                }
            }
            KeyCode::Char('f') => {
                self.overlay = Overlay::None;
                self.mode = Mode::Filter;
                self.reset_input();
            }
            KeyCode::Char('h') => {
                self.overlay = Overlay::None;
                self.mode = Mode::Highlight;
                self.reset_input();
            }
            KeyCode::Char('e') => self.export_view(),
            KeyCode::Char('g') => self.export_shell(),
            KeyCode::Char('s') => self.syntax_color = !self.syntax_color,
            KeyCode::Char('l') => self.color_filters = !self.color_filters,
            KeyCode::Char('z') => self.zebra = !self.zebra,
            _ => {}
        }
    }

    // Inline editor for the Separator / Hide Columns menu fields. Reuses
    // `self.input`; Enter commits, Esc cancels, editing keys mirror the prompt.
    fn handle_menu_edit_key(&mut self, key: KeyEvent) {
        let len = self.input.chars().count();
        match key.code {
            KeyCode::Enter => {
                let val = self.input.trim().to_string();
                match self.menu_edit {
                    Some(MenuField::Separator) => {
                        self.col_sep_text = if val.is_empty() { ",".to_string() } else { val };
                    }
                    Some(MenuField::Columns) => self.hide_cols_text = val,
                    Some(MenuField::Bookmarks) => {
                        // Comma-separated 1-based line numbers, clamped to the file.
                        self.bookmarks =
                            parse_cols(&val).into_iter().filter(|&n| n <= self.total).collect();
                        if self.displayed.is_some() {
                            self.refresh_preserving();
                        }
                    }
                    None => {}
                }
                self.menu_edit = None;
                self.reset_input();
            }
            KeyCode::Esc => {
                self.menu_edit = None;
                self.reset_input();
            }
            KeyCode::Backspace => {
                if self.input_cursor > 0 {
                    let b = self.input_byte(self.input_cursor - 1);
                    self.input.remove(b);
                    self.input_cursor -= 1;
                }
            }
            KeyCode::Delete => {
                if self.input_cursor < len {
                    let b = self.input_byte(self.input_cursor);
                    self.input.remove(b);
                }
            }
            KeyCode::Left => self.input_cursor = self.input_cursor.saturating_sub(1),
            KeyCode::Right => self.input_cursor = (self.input_cursor + 1).min(len),
            KeyCode::Home => self.input_cursor = 0,
            KeyCode::End => self.input_cursor = len,
            KeyCode::Char(c) => {
                let b = self.input_byte(self.input_cursor);
                self.input.insert(b, c);
                self.input_cursor += 1;
            }
            _ => {}
        }
    }

    fn current_view(&self) -> ViewState {
        ViewState {
            filters: self.filters.clone(),
            filter_mode: self.filter_mode,
            highlights: self.highlights.clone(),
            bookmarks: self.bookmarks.iter().copied().collect(),
            visible: self.visible_lines.iter().copied().collect(),
            hidden: self.hidden_lines.iter().copied().collect(),
            search: self.search_term.clone(),
            search_regex: self.search_regex,
            search_case: self.search_case,
            selected: self.current_line(),
            hide_cols: self.hide_cols_text.clone(),
            // Emit the separator only when it differs from the default comma.
            col_sep: if self.col_sep_text == "," { String::new() } else { self.col_sep_text.clone() },
            syntax: self.syntax_color,
            // The TUI has no level-chip UI; the web sets this. Kept default so the
            // token format stays single-sourced in view::state.
            levels: Vec::new(),
            // The TUI's histogram time range is runtime-only today; the web persists
            // its own in the token. Kept default here to avoid changing TUI behaviour.
            time_range: None,
        }
    }

    fn export_view(&mut self) {
        let s = serialize_view(&self.current_view());
        let body = if s.is_empty() {
            vec!["(no filters / highlights / bookmarks / ranges set)".to_string()]
        } else {
            vec![s]
        };
        self.overlay = Overlay::Text {
            title: "Shareable view — restore with: zerch <file> --view \"…\"".to_string(),
            body,
        };
        self.overlay_scroll = 0;
    }

    fn export_shell(&mut self) {
        let mut forced: Vec<usize> = self.bookmarks.union(&self.visible_lines).copied().collect();
        forced.sort_unstable();
        let filters: Vec<ShellFilter> = self
            .filters
            .iter()
            .filter(|f| f.enabled && !f.term.is_empty())
            .map(|f| ShellFilter {
                term: f.term.clone(),
                case_sensitive: f.case_sensitive,
                regex: f.regex,
                negate: f.negate,
            })
            .collect();
        let file = std::path::Path::new(&self.file_name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&self.file_name)
            .to_string();
        let cmd = build_shell_command(&ShellCommandOptions {
            filters,
            filter_mode: self.filter_mode,
            forced_lines: forced,
            file_name: file,
        });
        self.overlay = Overlay::Text {
            title: "Shell command (grep/awk) reproducing this view".to_string(),
            body: cmd.lines().map(|l| l.to_string()).collect(),
        };
        self.overlay_scroll = 0;
    }

    // Standalone hit count for a single filter/highlight term.
    fn compute_filter_count(&self, f: &FilterEntry) -> Option<usize> {
        if f.term.is_empty() {
            return None;
        }
        let stack = [SearchTerm::new(&f.term, Op::And)
            .case(f.case_sensitive)
            .rx(f.regex)
            .neg(f.negate)];
        search(&self.index, &stack, &Scope::default())
            .ok()
            .map(|r| r.total_matches)
    }

    fn compute_highlight_count(&self, h: &HighlightEntry) -> Option<usize> {
        if h.term.is_empty() {
            return None;
        }
        let stack = [SearchTerm::new(&h.term, Op::And)
            .case(h.case_sensitive)
            .rx(h.regex)];
        search(&self.index, &stack, &Scope::default())
            .ok()
            .map(|r| r.total_matches)
    }

    // Reset all counts to Pending. Cheap. The scheduler fills them in later.
    fn reset_counts_pending(&mut self) {
        self.filter_counts = vec![Count::Pending; self.filters.len()];
        self.highlight_counts = vec![Count::Pending; self.highlights.len()];
    }

    /// True while there's still at least one term left to scan.
    pub fn has_pending_counts(&self) -> bool {
        self.filter_counts.iter().any(|c| c.is_pending())
            || self.highlight_counts.iter().any(|c| c.is_pending())
    }

    /// Compute one pending count (one full scan). Called between frames from the
    /// run loop's idle time, so counts fill in visibly without ever blocking the
    /// first draw. Returns true if more work remains.
    pub fn compute_next_pending_count(&mut self) -> bool {
        for i in 0..self.filter_counts.len() {
            if self.filter_counts[i].is_pending() {
                self.filter_counts[i] =
                    Count::from_option(self.compute_filter_count(&self.filters[i]));
                return self.has_pending_counts();
            }
        }
        for j in 0..self.highlight_counts.len() {
            if self.highlight_counts[j].is_pending() {
                self.highlight_counts[j] =
                    Count::from_option(self.compute_highlight_count(&self.highlights[j]));
                return self.has_pending_counts();
            }
        }
        false
    }

    // One inline-editable menu field row: shows the value, or a live caret editor
    // when this field is being edited.
    fn menu_field_row(&self, label: &str, value: &str, selected: bool, field: MenuField) -> Line<'static> {
        let cur = if selected { "▸ " } else { "  " };
        if self.menu_edit == Some(field) {
            let mut spans = vec![
                Span::styled(cur.to_string(), Style::default().fg(Color::Yellow)),
                Span::raw(label.to_string()),
            ];
            spans.extend(self.input_spans(""));
            Line::from(spans)
        } else {
            let shown = if value.is_empty() { "(none)".to_string() } else { value.to_string() };
            let style = if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(Span::styled(format!("{cur}{label}{shown}"), style))
        }
    }

    fn render_menu(&mut self, f: &mut Frame) {
        let area = f.area();
        let block = Block::bordered().title(
            " Menu · space toggle · r rx · i case · x neg · d del · C/c/B/X clear filt/hl/bm/hidden · o AND/OR · f/h add · s syntax · l filter-color · z zebra · e view · g grep · q close ",
        );
        let inner = block.inner(area);
        let nf = self.filters.len();
        let mut lines: Vec<Line<'static>> = Vec::new();

        lines.push(Line::from(Span::styled(
            format!(
                "Filters ({})",
                if self.filter_mode == FilterMode::And { "AND" } else { "OR" }
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        if self.filters.is_empty() {
            lines.push(Line::from(Span::styled("  (none)", Style::default().fg(Color::DarkGray))));
        }
        for (i, filt) in self.filters.iter().enumerate() {
            lines.push(filter_row(
                filt,
                self.menu_sel == i,
                self.filter_counts.get(i).copied().unwrap_or(Count::Pending),
                &self.filter_color(i),
                self.color_filters,
            ));
        }

        lines.push(Line::from(String::new()));
        lines.push(Line::from(Span::styled(
            "Highlights",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        if self.highlights.is_empty() {
            lines.push(Line::from(Span::styled("  (none)", Style::default().fg(Color::DarkGray))));
        }
        for (j, hl) in self.highlights.iter().enumerate() {
            lines.push(highlight_row(
                hl,
                self.menu_sel == nf + j,
                self.highlight_counts.get(j).copied().unwrap_or(Count::Pending),
                &self.highlight_color(j),
            ));
        }

        let nh = self.highlights.len();
        lines.push(Line::from(String::new()));
        lines.push(Line::from(Span::styled(
            "Field hiding  (Enter to edit · … replaces hidden columns)",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(self.menu_field_row(
            "Separator:    ",
            &self.col_sep_text,
            self.menu_sel == nf + nh,
            MenuField::Separator,
        ));
        lines.push(self.menu_field_row(
            "Hide Columns: ",
            &self.hide_cols_text,
            self.menu_sel == nf + nh + 1,
            MenuField::Columns,
        ));

        lines.push(Line::from(String::new()));
        let bm_val: String =
            self.bookmarks.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(",");
        lines.push(self.menu_field_row(
            "Bookmarks:    ",
            &bm_val,
            self.menu_sel == nf + nh + 2,
            MenuField::Bookmarks,
        ));
        lines.push(Line::from(Span::styled(
            format!(
                "Hidden lines: {}  (x hides the cursor line; reveal the gap to undo · X clears)",
                self.hidden_lines.len()
            ),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(Span::styled(
            format!(
                "Syntax color: {}  (s toggles)",
                if self.syntax_color { "on" } else { "off" }
            ),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(Span::styled(
            format!(
                "Filter color: {}  (l toggles — boxes filter matches like highlights)",
                if self.color_filters { "on" } else { "off" }
            ),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(Span::styled(
            format!(
                "Zebra stripes: {}  (z toggles — faint tint on alternate lines)",
                if self.zebra { "on" } else { "off" }
            ),
            Style::default().fg(Color::DarkGray),
        )));

        // Clamp overlong menus to the visible area.
        let visible: Vec<Line> = lines.into_iter().take(inner.height as usize).collect();
        f.render_widget(Paragraph::new(visible).block(block), area);
    }

    // --- key handling ---------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        // Any keypress clears a transient hint from the previous action.
        self.hint = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            self.should_quit = true;
            return;
        }
        if ctrl && matches!(key.code, KeyCode::Char('z')) {
            self.suspend = true;
            return;
        }
        if self.confirm_quit {
            match key.code {
                // q again (or y) confirms; any other key cancels.
                KeyCode::Char('q') | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.should_quit = true
                }
                _ => self.confirm_quit = false,
            }
            return;
        }
        match self.overlay {
            Overlay::None => {}
            Overlay::Menu => {
                self.handle_menu_key(key);
                return;
            }
            _ => {
                self.handle_scroll_overlay_key(key);
                return;
            }
        }
        if self.mode == Mode::Hist {
            self.handle_hist_key(key);
            return;
        }
        if self.mode != Mode::Normal {
            self.handle_input_key(key, ctrl);
            return;
        }
        self.handle_normal_key(key, ctrl);
    }

    fn handle_input_key(&mut self, key: KeyEvent, ctrl: bool) {
        if ctrl {
            match key.code {
                KeyCode::Char('r') => match self.mode {
                    Mode::Search => self.search_regex = !self.search_regex,
                    Mode::Highlight | Mode::Filter => self.input_regex = !self.input_regex,
                    _ => {}
                },
                KeyCode::Char('a') => match self.mode {
                    Mode::Search => self.search_case = !self.search_case,
                    Mode::Highlight | Mode::Filter => self.input_case = !self.input_case,
                    _ => {}
                },
                _ => {}
            }
            return;
        }
        let len = self.input.chars().count();
        match key.code {
            KeyCode::Enter => {
                let term = self.input.trim().to_string();
                match self.mode {
                    Mode::Search => self.run_search(&term),
                    Mode::Highlight => {
                        self.add_highlight(&term);
                        self.mode = Mode::Normal;
                    }
                    Mode::Filter => {
                        self.add_filter(&term);
                        self.mode = Mode::Normal;
                    }
                    Mode::Command => self.run_command(&term),
                    Mode::Normal | Mode::Hist => {}
                }
                self.reset_input();
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.reset_input();
            }
            KeyCode::Backspace => {
                if self.input.is_empty() {
                    self.mode = Mode::Normal;
                } else if self.input_cursor > 0 {
                    let b = self.input_byte(self.input_cursor - 1);
                    self.input.remove(b);
                    self.input_cursor -= 1;
                }
                self.hist_idx = -1;
            }
            KeyCode::Delete => {
                if self.input_cursor < len {
                    let b = self.input_byte(self.input_cursor);
                    self.input.remove(b);
                }
                self.hist_idx = -1;
            }
            KeyCode::Left => self.input_cursor = self.input_cursor.saturating_sub(1),
            KeyCode::Right => self.input_cursor = (self.input_cursor + 1).min(len),
            KeyCode::Home => self.input_cursor = 0,
            KeyCode::End => self.input_cursor = len,
            // History recall is for the term prompts; the `:` command line has none.
            KeyCode::Up if self.mode != Mode::Command => self.history_recall(1),
            KeyCode::Down if self.mode != Mode::Command => self.history_recall(-1),
            KeyCode::Char(c) => {
                let b = self.input_byte(self.input_cursor);
                self.input.insert(b, c);
                self.input_cursor += 1;
                self.hist_idx = -1;
            }
            _ => {}
        }
    }

    // Byte offset of the `n`-th char in `input` (end if out of range).
    fn input_byte(&self, n: usize) -> usize {
        self.input
            .char_indices()
            .nth(n)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len())
    }

    fn reset_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.hist_idx = -1;
        self.input_regex = false;
        self.input_case = false;
    }

    fn history_recall(&mut self, dir: i64) {
        let len = self.history.len() as i64;
        if len == 0 {
            return;
        }
        if dir > 0 {
            let ni = (self.hist_idx + 1).min(len - 1);
            self.hist_idx = ni;
            self.input = self.history[ni as usize].clone();
        } else if self.hist_idx <= 0 {
            self.hist_idx = -1;
            self.input.clear();
        } else {
            let ni = self.hist_idx - 1;
            self.hist_idx = ni;
            self.input = self.history[ni as usize].clone();
        }
        self.input_cursor = self.input.chars().count(); // caret to end of recalled term
    }

    fn handle_normal_key(&mut self, key: KeyEvent, ctrl: bool) {
        // Numeric prefix: digits accumulate into `pending_count` (used as the pan
        // distance). A bare `0` (nothing pending) keeps its meaning: reset pan.
        if !ctrl {
            if let KeyCode::Char(c) = key.code {
                if c.is_ascii_digit() && !(c == '0' && self.pending_count.is_none()) {
                    let d = (c as u8 - b'0') as usize;
                    self.pending_count =
                        Some(self.pending_count.unwrap_or(0).saturating_mul(10).saturating_add(d));
                    return;
                }
            }
        }
        // Any other key consumes (and clears) a pending prefix; only pan uses it.
        let prefix = self.pending_count.take();
        if ctrl {
            match key.code {
                KeyCode::Char('f') => self.page_by(1),
                KeyCode::Char('b') => self.page_by(-1),
                _ => {}
            }
            return;
        }
        let count = self.count() as i64;
        match key.code {
            KeyCode::Char('q') => self.confirm_quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char('f') | KeyCode::Char(' ') | KeyCode::PageDown => self.page_by(1),
            KeyCode::Char('b') | KeyCode::PageUp => self.page_by(-1),
            KeyCode::Char('g') => {
                self.cursor = 0;
                self.pos = 0;
            }
            KeyCode::Char('G') => self.jump_to(count - 1),
            KeyCode::Char('H') => self.cursor_to_screen(ScreenPos::Top),
            KeyCode::Char('M') => self.cursor_to_screen(ScreenPos::Middle),
            KeyCode::Char('L') => self.cursor_to_screen(ScreenPos::Bottom),
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.reset_input();
            }
            KeyCode::Left => self.pan(-1, prefix),
            KeyCode::Right => self.pan(1, prefix),
            KeyCode::Char('0') => self.hscroll = 0,
            KeyCode::Char('w') => {
                self.wrap = !self.wrap;
                self.hscroll = 0;
                self.adjust_pos_for_wrap_toggle();
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.reset_input();
            }
            KeyCode::Char('h') => {
                self.mode = Mode::Highlight;
                self.reset_input();
            }
            KeyCode::Char('F') => {
                self.mode = Mode::Filter;
                self.reset_input();
            }
            KeyCode::Char('n') => self.goto_match_relative(true),
            KeyCode::Char('N') => self.goto_match_relative(false),
            KeyCode::Char('B') => self.toggle_bookmark(),
            KeyCode::Char('x') => self.hide_current_line(),
            KeyCode::Char('[') => self.goto_bookmark(false),
            KeyCode::Char(']') => self.goto_bookmark(true),
            KeyCode::Char('o') => self.toggle_filter_mode(),
            KeyCode::Enter => self.enter_action(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.gap_action(true),
            KeyCode::Char('-') | KeyCode::Char('_') => self.gap_action(false),
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
                self.overlay_scroll = 0;
            }
            KeyCode::Char('m') => {
                self.overlay = Overlay::Menu;
                self.menu_sel = 0;
            }
            KeyCode::Char('T') => self.toggle_hist(),
            KeyCode::Char('t') => {
                if self.show_hist {
                    self.mode = Mode::Hist;
                }
            }
            KeyCode::Esc => {
                self.search_term.clear();
                self.matches.clear();
                self.search_error = None;
                self.refresh_matches_in_view();
            }
            _ => {}
        }
    }

    // --- rendering ------------------------------------------------------------

    pub fn render(&mut self, f: &mut Frame) {
        match self.overlay {
            Overlay::None => {}
            Overlay::Menu => {
                self.render_menu(f);
                return;
            }
            _ => {
                self.render_overlay(f);
                return;
            }
        }
        let area = f.area();
        let status_style = Style::default().add_modifier(Modifier::REVERSED);
        if self.show_hist {
            let ch = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
            self.ensure_hist(ch[0].width as usize);
            f.render_widget(Paragraph::new(self.hist_line()), ch[0]);
            self.width = ch[1].width;
            self.height = ch[1].height as usize;
            f.render_widget(Paragraph::new(self.build_rows()), ch[1]);
            f.render_widget(Paragraph::new(self.status_line()).style(status_style), ch[2]);
            f.render_widget(Paragraph::new(self.prompt_line()), ch[3]);
        } else {
            let ch = Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
            self.width = ch[0].width;
            self.height = ch[0].height as usize;
            f.render_widget(Paragraph::new(self.build_rows()), ch[0]);
            f.render_widget(Paragraph::new(self.status_line()).style(status_style), ch[1]);
            f.render_widget(Paragraph::new(self.prompt_line()), ch[2]);
        }
    }

    fn gutter_width(&self) -> usize {
        self.total.to_string().len().max(2)
    }

    // Highlights and filters share one colour pool (HIGHLIGHT_PALETTE), assigned by
    // position so the two don't collide: highlight i takes slot i, filter j takes slot
    // (num_highlights + j). Both are indexed over the full list, so enabling/disabling
    // an item never shifts another's colour. (They can only repeat past the palette's
    // length.) Colours are positional — the same scheme the web UI / `parse_view` use.
    fn highlight_color(&self, i: usize) -> String {
        HIGHLIGHT_PALETTE[i % HIGHLIGHT_PALETTE.len()].to_string()
    }
    fn filter_color(&self, j: usize) -> String {
        HIGHLIGHT_PALETTE[(self.highlights.len() + j) % HIGHLIGHT_PALETTE.len()].to_string()
    }

    // Specs used to colour a rendered line: the `h` highlights first, then the active
    // filter terms (so a real highlight wins over a filter on overlap, and search — a
    // separate overlay — wins over both). A filtered-for term is boxed in its palette
    // colour, so filters are highlighted in place, mirroring the web UI.
    fn enabled_highlight_specs(&self) -> Vec<HighlightSpec> {
        let mut specs: Vec<HighlightSpec> = self
            .highlights
            .iter()
            .enumerate()
            .filter(|(_, h)| h.enabled && !h.term.is_empty())
            .map(|(i, h)| HighlightSpec {
                term: h.term.clone(),
                color: self.highlight_color(i),
                case_sensitive: h.case_sensitive,
                regex: h.regex,
            })
            .collect();
        // Enabled, non-negated filters (a negation excludes rather than matches, so
        // there's nothing to colour). Bad regexes are skipped like in `recompute`.
        // Skipped entirely when filter colouring is toggled off.
        if self.color_filters {
            specs.extend(
                self.filters
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| {
                        f.enabled && !f.term.is_empty() && !f.negate && !(f.regex && bad_regex(&f.term))
                    })
                    .map(|(j, f)| HighlightSpec {
                        term: f.term.clone(),
                        color: self.filter_color(j),
                        case_sensitive: f.case_sensitive,
                        regex: f.regex,
                    }),
            );
        }
        specs
    }

    // Per-character styled content for one line: syntax base (opt-in) with
    // highlight boxes and the search overlay layered on top. Precedence:
    // search > highlight > syntax > plain.
    fn col_hide_active(&self) -> bool {
        !parse_cols(&self.hide_cols_text).is_empty()
    }

    // Replace the configured 1-based columns with "…", splitting on the chosen
    // separator. Char separators preserve the delimiter; whitespace mode collapses
    // runs to single spaces. Applied before highlight/search/syntax so their
    // offsets line up with what's shown.
    fn apply_col_hide(&self, raw: &str) -> String {
        let cols = parse_cols(&self.hide_cols_text);
        if cols.is_empty() {
            return raw.to_string();
        }
        let blank = |i: usize, part: &str| -> String {
            if cols.contains(&(i + 1)) { "…".to_string() } else { part.to_string() }
        };
        match parse_sep(&self.col_sep_text) {
            FieldSep::Char(c) => raw
                .split(c)
                .enumerate()
                .map(|(i, p)| blank(i, p))
                .collect::<Vec<_>>()
                .join(&c.to_string()),
            FieldSep::Whitespace => raw
                .split_whitespace()
                .enumerate()
                .map(|(i, p)| blank(i, p))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    fn style_line(&self, n: usize, hide_cols: bool) -> Vec<(char, Style)> {
        let line = self.index.get_line(n);
        let base = if hide_cols && self.col_hide_active() {
            self.apply_col_hide(&line)
        } else {
            line.into_owned()
        };
        let raw = base.replace('\t', "  ");
        let specs = self.enabled_highlight_specs();
        let spans = build_spans(
            &raw,
            &specs,
            &self.search_term,
            self.search_case,
            self.search_regex,
        );

        // Per-char syntax base colour (only computed when the toggle is on).
        let syn: Vec<Option<Style>> = if self.syntax_color {
            let total = raw.chars().count();
            let mut v: Vec<Option<Style>> = vec![None; total];
            for (s, e, tok) in syntax_ranges(&raw) {
                let style = Style::default().fg(syntax_tok_color(tok));
                for slot in v[s.min(total)..e.min(total)].iter_mut() {
                    *slot = Some(style);
                }
            }
            v
        } else {
            Vec::new()
        };

        let mut out: Vec<(char, Style)> = Vec::with_capacity(raw.len());
        let mut ci: usize = 0;
        for s in spans {
            let plain = !s.search && s.color.is_none();
            let base = if s.search {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else if let Some(c) = s.color {
                // highlight: a coloured box (like search), one colour per term
                let (r, g, b) = hex_rgb(&c);
                Style::default().fg(Color::Black).bg(Color::Rgb(r, g, b))
            } else {
                Style::default()
            };
            for (i, ch) in s.text.chars().enumerate() {
                let style = if plain && !syn.is_empty() {
                    syn.get(ci + i).copied().flatten().unwrap_or(base)
                } else {
                    base
                };
                out.push((ch, style));
            }
            ci += s.text.chars().count();
        }
        out
    }

    fn build_rows(&self) -> Vec<Line<'static>> {
        let count = self.count();
        if count == 0 {
            return vec![Line::from(Span::styled(
                "(empty file)",
                Style::default().fg(Color::DarkGray),
            ))];
        }
        let gutter = self.gutter_width();
        let content_w = self.content_width();

        let mut rows: Vec<Line<'static>> = Vec::new();
        let mut p = self.pos;
        while rows.len() < self.height && p < count {
            let is_cursor = p == self.cursor;
            let stripe = self.zebra && p % 2 == 1;
            match self.item_at(p) {
                DisplayedItem::Line(n) => {
                    self.push_line_rows(&mut rows, n, is_cursor, stripe, gutter, content_w);
                }
                DisplayedItem::Gap { start, end } => {
                    rows.push(self.gap_row(start, end, is_cursor, gutter));
                }
            }
            p += 1;
        }
        rows
    }

    fn push_line_rows(
        &self,
        rows: &mut Vec<Line<'static>>,
        n: usize,
        is_cursor: bool,
        stripe: bool,
        gutter: usize,
        content_w: usize,
    ) {
        let styled = self.style_line(n, true);
        let num_style = if self.bookmarks.contains(&n) {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            match level_rgb(self.index.level_of(n)) {
                Some((r, g, b)) => Style::default().fg(Color::Rgb(r, g, b)),
                None => Style::default().fg(Color::DarkGray),
            }
        };
        let num = format!("{n:>gutter$}");
        let blank = " ".repeat(gutter);
        let row_style = if is_cursor {
            Style::default().bg(CURSOR_BG)
        } else if stripe {
            Style::default().bg(ZEBRA_BG)
        } else {
            Style::default()
        };
        // Cursor and zebra rows carry a background, so pad the content area with
        // trailing spaces to the full width — otherwise the tint stops at the last
        // character instead of running to the right edge.
        let pad = is_cursor || stripe;

        if self.wrap {
            if styled.is_empty() {
                let mut spans = vec![Span::styled(num, num_style), Span::raw(" ")];
                if pad {
                    spans.push(Span::raw(" ".repeat(content_w)));
                }
                rows.push(Line::from(spans).style(row_style));
                return;
            }
            let mut ci = 0;
            let mut first = true;
            while ci < styled.len() && rows.len() < self.height {
                let end = (ci + content_w).min(styled.len());
                let g = if first {
                    Span::styled(num.clone(), num_style)
                } else {
                    Span::raw(blank.clone())
                };
                let mut spans = vec![g, Span::raw(" ")];
                spans.extend(group_spans(&styled[ci..end]));
                if pad {
                    let shown = end - ci;
                    if shown < content_w {
                        spans.push(Span::raw(" ".repeat(content_w - shown)));
                    }
                }
                rows.push(Line::from(spans).style(row_style));
                ci = end;
                first = false;
            }
        } else {
            let start = self.hscroll.min(styled.len());
            let end = (start + content_w).min(styled.len());
            let mut spans = vec![Span::styled(num, num_style), Span::raw(" ")];
            spans.extend(group_spans(&styled[start..end]));
            if pad {
                let shown = end - start;
                if shown < content_w {
                    spans.push(Span::raw(" ".repeat(content_w - shown)));
                }
            }
            rows.push(Line::from(spans).style(row_style));
        }
    }

    fn gap_row(&self, start: usize, end: usize, is_cursor: bool, gutter: usize) -> Line<'static> {
        let hidden = end - start + 1;
        let txt = format!(
            "{}··· {hidden} lines hidden ({start}–{end}) ···",
            " ".repeat(gutter + 1)
        );
        let style = if is_cursor {
            Style::default().fg(Color::DarkGray).bg(CURSOR_BG)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        Line::from(Span::styled(txt, style))
    }

    fn status_line(&self) -> Line<'static> {
        let count = self.count();
        let here = if count == 0 {
            "0".to_string()
        } else {
            match self.item_at(self.cursor.min(count - 1)) {
                DisplayedItem::Line(n) => n.to_string(),
                DisplayedItem::Gap { .. } => "—".to_string(),
            }
        };
        let mut s = format!(" {}   {}/{}   ", self.file_name, here, self.total);
        if !self.search_term.is_empty() {
            let miv = self.miv.len();
            match self.search_error {
                Some(e) => s.push_str(&format!("/{}: {}  ", self.search_term, e)),
                None => s.push_str(&format!("/{} {}/{}  ", self.search_term, self.match_idx + 1, miv)),
            }
        }
        if self.wrap {
            s.push_str("wrap  ");
        }
        if self.hscroll > 0 {
            s.push_str(&format!("col {}  ", self.hscroll));
        }
        if let Some(n) = self.pending_count {
            s.push_str(&format!("{n}›  ")); // digits typed, waiting for ←/→
        }
        if let Some(h) = self.hint {
            s.push_str(&format!(" · {h}"));
        }
        Line::from(s)
    }

    fn rx_badge(&self) -> &'static str {
        if self.input_regex {
            ".*"
        } else {
            "ab"
        }
    }
    fn ca_badge(&self) -> &'static str {
        if self.input_case {
            "Aa"
        } else {
            "aa"
        }
    }

    // Render the input line as `prefix` + text with a block caret at input_cursor.
    fn input_spans(&self, prefix: &str) -> Vec<Span<'static>> {
        let chars: Vec<char> = self.input.chars().collect();
        let cur = self.input_cursor.min(chars.len());
        let before: String = chars[..cur].iter().collect();
        let (under, after): (String, String) = if cur < chars.len() {
            (chars[cur].to_string(), chars[cur + 1..].iter().collect())
        } else {
            (" ".to_string(), String::new())
        };
        vec![
            Span::raw(format!("{prefix}{before}")),
            Span::styled(under, Style::default().add_modifier(Modifier::REVERSED)),
            Span::raw(after),
        ]
    }

    fn prompt_line(&self) -> Line<'static> {
        if self.confirm_quit {
            return Line::from(Span::styled(
                "Quit? press q or y to confirm · any other key cancels",
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        let dim = Style::default().fg(Color::DarkGray);
        match self.mode {
            Mode::Search => {
                let flags = format!(
                    "  [{} {}] ^R rx · ^A case · ↑↓ hist",
                    if self.search_regex { ".*" } else { "ab" },
                    if self.search_case { "Aa" } else { "aa" }
                );
                let mut spans = self.input_spans("/");
                spans.push(Span::styled(flags, dim));
                Line::from(spans)
            }
            Mode::Highlight => {
                let mut spans = self.input_spans("highlight: ");
                spans.push(Span::styled(
                    format!("  [{} {}] ^R rx · ^A case · ↑↓ recent", self.rx_badge(), self.ca_badge()),
                    dim,
                ));
                Line::from(spans)
            }
            Mode::Filter => {
                let mut spans = self.input_spans("filter: ");
                spans.push(Span::styled(
                    format!(
                        "  [{}] [{} {}] ^R rx · ^A case · ↑↓ recent",
                        if self.filter_mode == FilterMode::And { "AND" } else { "OR" },
                        self.rx_badge(),
                        self.ca_badge()
                    ),
                    dim,
                ));
                Line::from(spans)
            }
            Mode::Command => {
                let mut spans = self.input_spans(":");
                spans.push(Span::styled("  line number · $ = last · q = quit · Enter", dim));
                Line::from(spans)
            }
            Mode::Hist => Line::from(Span::styled(
                "time brush: ←/→ move · a anchor · Enter apply · c clear · T/Esc close",
                Style::default().fg(Color::DarkGray),
            )),
            Mode::Normal => Line::from(Span::styled(HINT, Style::default().fg(Color::DarkGray))),
        }
    }
}

// Group consecutive equally-styled chars into ratatui spans.
fn group_spans(slice: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut cur_style: Option<Style> = None;
    for &(ch, st) in slice {
        if cur_style == Some(st) {
            cur.push(ch);
        } else {
            if let Some(s) = cur_style {
                spans.push(Span::styled(std::mem::take(&mut cur), s));
            }
            cur.push(ch);
            cur_style = Some(st);
        }
    }
    if let Some(s) = cur_style {
        spans.push(Span::styled(cur, s));
    }
    spans
}

// Wrap per-character styled content into fixed-width rows, grouping runs of equal
// style into spans.
fn wrap_styled(styled: &[(char, Style)], w: usize) -> Vec<Line<'static>> {
    if styled.is_empty() {
        return vec![Line::from(String::new())];
    }
    let w = w.max(1);
    let mut out = Vec::new();
    let mut i = 0;
    while i < styled.len() {
        let end = (i + w).min(styled.len());
        out.push(Line::from(group_spans(&styled[i..end])));
        i = end;
    }
    out
}

fn wrap_plain(s: &str, w: usize) -> Vec<Line<'static>> {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return vec![Line::from(String::new())];
    }
    let w = w.max(1);
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let end = (i + w).min(chars.len());
        out.push(Line::from(chars[i..end].iter().collect::<String>()));
        i = end;
    }
    out
}

fn syntax_tok_color(t: SyntaxTok) -> Color {
    match t {
        SyntaxTok::Key => Color::Rgb(0x4f, 0xc3, 0xf7),  // cyan-blue
        SyntaxTok::Str => Color::Rgb(0x81, 0xc7, 0x84),  // green
        SyntaxTok::Num => Color::Rgb(0xff, 0xb7, 0x4d),  // amber
        SyntaxTok::Bool => Color::Rgb(0xba, 0x68, 0xc8), // purple
        SyntaxTok::Null => Color::Rgb(0x9e, 0x9e, 0x9e), // gray
    }
}

fn name_color(name: &str) -> Color {
    match name {
        "cyan" => Color::Cyan,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "magenta" => Color::Magenta,
        _ => Color::DarkGray,
    }
}

fn help_lines() -> Vec<Line<'static>> {
    const ROWS: &[&str] = &[
        "Movement   j/k or ↑/↓ move · f/b (or space/PgDn, ^F/^B) page · g/G top/bottom",
        "           H/M/L cursor to screen top/middle/bottom · :N go to line N · :$ last · :q quit",
        "Long lines ←/→ pan (half screen) · N←/→ pan N columns · 0 reset pan · w in-place wrap",
        "Search     / search · n/N next/prev (from cursor) · ^R regex · ^A case · ↑/↓ history · Esc clear",
        "Highlight  h add a coloured highlight term",
        "Filter     F add filter · o toggle AND/OR",
        "Bookmarks  B toggle on cursor line · [ / ] previous/next bookmark",
        "Hide       x hide the cursor line (collapses to a gap) · reveal the gap or menu X to undo",
        "Gaps       Enter/+ reveal from top · - reveal from bottom",
        "Expand     Enter on a line: JSON view, or wrapped view if it doesn't fit",
        "Other      ? this help · q quit (confirm) · ^C quit now",
    ];
    ROWS.iter()
        .map(|r| Line::from(Span::raw(r.to_string())))
        .collect()
}

// Format a hit count with thousands separators.
// "…" = still pending (background scan hasn't reached this term yet).
// "—" = failed (bad regex, empty term).
fn fmt_count(c: Count) -> String {
    match c {
        Count::Value(n) => {
            let s = n.to_string();
            let bytes = s.as_bytes();
            let mut out = String::with_capacity(s.len() + s.len() / 3);
            for (i, b) in bytes.iter().enumerate() {
                if i > 0 && (bytes.len() - i) % 3 == 0 {
                    out.push(',');
                }
                out.push(*b as char);
            }
            format!("{out:>9}")
        }
        Count::Pending => format!("{:>9}", "…"),
        Count::Failed => format!("{:>9}", "—"),
    }
}

// `color` is the item's palette colour (from the shared pool); `show_color` is false
// when filter colouring is toggled off, so the swatch is dimmed to show it's inactive.
fn filter_row(f: &FilterEntry, selected: bool, count: Count, color: &str, show_color: bool) -> Line<'static> {
    let cur = if selected { "▸ " } else { "  " };
    let en = if f.enabled { "[on]" } else { "[· ]" };
    let rx = if f.regex { ".*" } else { "ab" };
    let ca = if f.case_sensitive { "Aa" } else { "aa" };
    let ng = if f.negate { " ≠" } else { "" };
    let bad = f.regex && bad_regex(&f.term);
    let cnt = fmt_count(if bad { Count::Failed } else { count });
    let mut st = if f.enabled {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    if bad {
        st = st.fg(Color::Red);
    }
    if selected {
        st = st.bg(CURSOR_BG);
    }
    // No swatch for a negated filter (it excludes, so nothing is coloured) or when
    // filter colouring is off.
    let swatch_span = if show_color && !f.negate {
        let (r, g, b) = hex_rgb(color);
        Span::styled("██", Style::default().fg(Color::Rgb(r, g, b)))
    } else {
        Span::styled("··", Style::default().fg(Color::DarkGray))
    };
    Line::from(vec![
        Span::styled(format!("{cur}{en} "), st),
        swatch_span,
        Span::styled(
            format!(" {rx} {ca}{ng} {cnt}  {}{}", f.term, if bad { " (bad regex)" } else { "" }),
            st,
        ),
    ])
}

fn highlight_row(h: &HighlightEntry, selected: bool, count: Count, color: &str) -> Line<'static> {
    let cur = if selected { "▸ " } else { "  " };
    let en = if h.enabled { "[on]" } else { "[· ]" };
    let rx = if h.regex { ".*" } else { "ab" };
    let ca = if h.case_sensitive { "Aa" } else { "aa" };
    let (r, g, b) = hex_rgb(color);
    let bad = h.regex && bad_regex(&h.term);
    let cnt = fmt_count(if bad { Count::Failed } else { count });
    let mut st = if h.enabled {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    if selected {
        st = st.bg(CURSOR_BG);
    }
    Line::from(vec![
        Span::styled(format!("{cur}{en} "), st),
        Span::styled("██", Style::default().fg(Color::Rgb(r, g, b))),
        Span::styled(format!(" {rx} {ca} {cnt}  {}", h.term), st),
    ])
}

fn bad_regex(pattern: &str) -> bool {
    regex::Regex::new(pattern).is_err()
}

fn push_history(history: &mut Vec<String>, term: &str) {
    if term.is_empty() {
        return;
    }
    history.retain(|t| t != term);
    history.insert(0, term.to_string());
    history.truncate(HISTORY_MAX);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::index::IndexOptions;
    use std::fs::File;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn parse_cols_handles_ranges() {
        assert_eq!(parse_cols("1,2,5"), vec![1, 2, 5]);
        assert_eq!(parse_cols("1-7"), vec![1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(parse_cols("1,3-5,8"), vec![1, 3, 4, 5, 8]);
        // overlaps and duplicates collapse; result stays sorted+deduped
        assert_eq!(parse_cols("3-5,4-6"), vec![3, 4, 5, 6]);
        // single-element range
        assert_eq!(parse_cols("4-4"), vec![4]);
        // reversed range is ignored; the good token survives
        assert_eq!(parse_cols("9-2,3"), vec![3]);
        // whitespace and junk tokens are tolerated
        assert_eq!(parse_cols(" 2 - 4 , x , 7 "), vec![2, 3, 4, 7]);
        // a huge range is capped, not expanded to millions of entries
        assert_eq!(parse_cols("1-999999").len(), 4096);
    }

    fn app_from(lines: &[&str]) -> App {
        static C: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "zerch_app_{}_{}.log",
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        File::create(&p)
            .unwrap()
            .write_all(lines.join("\n").as_bytes())
            .unwrap();
        let idx = LineIndex::open(&p, IndexOptions::default(), |_| {}).unwrap();
        let _ = std::fs::remove_file(&p);
        let mut app = App::new(idx, "test.log".into());
        app.height = 10;
        app.width = 80;
        app
    }

    fn app_n(n: usize) -> App {
        let lines: Vec<String> = (1..=n).map(|i| format!("line {i}")).collect();
        app_from(&lines.iter().map(String::as_str).collect::<Vec<_>>())
    }

    #[test]
    fn filter_terms_are_highlighted_like_h() {
        let mut app = app_from(&["error here", "ok"]);
        app.highlights.push(HighlightEntry {
            term: "here".into(),
            color: HIGHLIGHT_PALETTE[0].to_string(),
            enabled: true,
            case_sensitive: false,
            regex: false,
        });
        app.filters.push(FilterEntry {
            term: "error".into(),
            enabled: true,
            case_sensitive: false,
            regex: false,
            negate: false,
        });
        // A disabled and a negated filter must NOT be coloured.
        app.filters.push(FilterEntry {
            term: "off".into(),
            enabled: false,
            case_sensitive: false,
            regex: false,
            negate: false,
        });
        app.filters.push(FilterEntry {
            term: "not".into(),
            enabled: true,
            case_sensitive: false,
            regex: false,
            negate: true,
        });
        let specs = app.enabled_highlight_specs();
        // Highlights come first (palette slot 0), then the active filter takes the next
        // shared-pool slot (after the one highlight) — no colour overlap.
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].term, "here");
        assert_eq!(specs[0].color, HIGHLIGHT_PALETTE[0]);
        assert_eq!(specs[1].term, "error");
        assert_eq!(specs[1].color, HIGHLIGHT_PALETTE[1]); // 1 highlight + filter index 0

        // The toggle removes filter colouring but keeps highlights.
        app.color_filters = false;
        let specs = app.enabled_highlight_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].term, "here");
    }

    #[test]
    fn double_q_and_ctrl_c_quit() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let q = || KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);

        let mut app = app_n(5);
        app.handle_key(q());
        assert!(!app.should_quit); // first q → confirm prompt
        app.handle_key(q());
        assert!(app.should_quit); // second q → exit

        // A stray key between the two cancels the confirm (no accidental quit).
        let mut app = app_n(5);
        app.handle_key(q());
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(!app.should_quit);

        // Ctrl-C exits immediately from normal mode.
        let mut app = app_n(5);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn move_page_jump() {
        let mut app = app_n(100);
        app.move_cursor(-5);
        assert_eq!(app.cursor, 0);
        app.page_by(1);
        assert_eq!(app.pos, 10);
        app.jump_to(1000);
        assert_eq!(app.cursor, 99);
    }

    #[test]
    fn wrap_moves_cursor_before_scrolling() {
        let mut app = app_from(&["a", "b", "c", "d", "e", "f"]);
        app.wrap = true;
        app.width = 80;
        app.height = 3; // short lines → 1 wrapped row each, 3 visible
        app.move_cursor(1);
        assert_eq!((app.cursor, app.pos), (1, 0)); // highlight moves down, no scroll
        app.move_cursor(1);
        assert_eq!((app.cursor, app.pos), (2, 0));
        app.move_cursor(1);
        assert_eq!((app.cursor, app.pos), (3, 1)); // now scrolls by one
        app.move_cursor(-3);
        assert_eq!((app.cursor, app.pos), (0, 0)); // scrolls back up
    }

    #[test]
    fn search_finds_and_jumps() {
        let mut app = app_from(&["alpha", "beta ERROR", "gamma", "delta ERROR"]);
        app.run_search("ERROR");
        assert_eq!(app.matches, vec![2, 4]);
        assert_eq!(app.cursor, 1); // jumped to first match (line 2 → pos 1)
        app.goto_match_relative(true); // n: next match after cursor
        assert_eq!(app.cursor, 3); // line 4
        app.goto_match_relative(true); // n: past the end → wraps to first
        assert_eq!(app.cursor, 1); // line 2
        app.goto_match_relative(false); // N: before first → wraps to last
        assert_eq!(app.cursor, 3); // line 4
    }

    #[test]
    fn screen_relative_cursor_moves() {
        let mut app = app_n(100);
        app.height = 10; // items 10..=19 visible after paging
        app.page_by(1);
        assert_eq!(app.pos, 10);
        app.cursor_to_screen(ScreenPos::Top);
        assert_eq!(app.cursor, 10); // top of screen
        app.cursor_to_screen(ScreenPos::Bottom);
        assert_eq!(app.cursor, 19); // bottom of the 10-row screen
        app.cursor_to_screen(ScreenPos::Middle);
        assert_eq!(app.cursor, 14); // (10+19)/2
        assert_eq!(app.pos, 10); // H/M/L never scroll
    }

    #[test]
    fn command_goto_line() {
        let mut app = app_n(100);
        app.height = 10;
        app.run_command("42");
        assert_eq!(app.current_line(), Some(42)); // 1-based document line
        app.run_command("$");
        assert_eq!(app.current_line(), Some(100)); // last line
        app.run_command("1");
        assert_eq!(app.current_line(), Some(1));
        // Out-of-range clamps; junk is rejected with a hint (no move).
        app.run_command("99999");
        assert_eq!(app.current_line(), Some(100));
        app.run_command("nope");
        assert_eq!(app.current_line(), Some(100));
        assert!(app.hint.is_some());
    }

    #[test]
    fn command_quit() {
        let mut app = app_n(10);
        app.run_command("q");
        assert!(app.should_quit);
        let mut app = app_n(10);
        app.run_command("quit");
        assert!(app.should_quit);
    }

    #[test]
    fn command_goto_reveals_filtered_line() {
        let mut app = app_from(&["aa", "bb", "cc aa", "dd", "ee aa"]);
        app.add_filter("aa"); // view now shows only lines 1,3,5 (+gaps)
        assert!(app.pos_of_line(4).is_none()); // line 4 filtered out
        app.run_command("4");
        assert_eq!(app.current_line(), Some(4)); // revealed and jumped to
    }

    #[test]
    fn count_prefix_pans_by_columns() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_from(&[
            "a very long line with plenty of characters to allow horizontal panning here",
        ]);
        app.width = 20;
        let press = |app: &mut App, c: char| {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
        };
        let right = |app: &mut App| app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));

        press(&mut app, '1');
        assert_eq!(app.pending_count, Some(1));
        right(&mut app);
        assert_eq!(app.hscroll, 1); // panned exactly one column
        assert_eq!(app.pending_count, None); // prefix consumed

        press(&mut app, '1');
        press(&mut app, '2');
        assert_eq!(app.pending_count, Some(12));
        right(&mut app);
        assert_eq!(app.hscroll, 13);

        press(&mut app, '0'); // bare 0 resets pan, not a count digit
        assert_eq!(app.pending_count, None);
        assert_eq!(app.hscroll, 0);

        press(&mut app, '5'); // a pending prefix is cleared by a non-pan key
        assert_eq!(app.pending_count, Some(5));
        press(&mut app, 'j');
        assert_eq!(app.pending_count, None);
    }

    #[test]
    fn hide_collapses_line_and_reveal_undoes() {
        let mut app = app_n(10);
        app.jump_to(4); // cursor on line 5
        assert_eq!(app.current_line(), Some(5));
        app.hide_current_line();
        assert!(app.hidden_lines.contains(&5));
        assert!(app.pos_of_line(5).is_none()); // collapsed into a gap
        let has_gap = app
            .displayed
            .as_ref()
            .unwrap()
            .iter()
            .any(|it| matches!(it, DisplayedItem::Gap { start, end } if *start <= 5 && *end >= 5));
        assert!(has_gap);
        // Revealing the gap un-hides the line and returns to the full view.
        app.gap_action(true);
        assert!(app.hidden_lines.is_empty());
        assert!(app.pos_of_line(5).is_some());
        assert!(app.displayed.is_none());
    }

    #[test]
    fn menu_clear_hidden() {
        let mut app = app_n(10);
        app.jump_to(2);
        app.hide_current_line();
        app.jump_to(6);
        app.hide_current_line();
        assert_eq!(app.hidden_lines.len(), 2);
        app.overlay = Overlay::Menu;
        app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE));
        assert!(app.hidden_lines.is_empty());
    }

    #[test]
    fn hidden_round_trips_through_view() {
        let mut app = app_n(10);
        app.jump_to(4);
        app.hide_current_line();
        let v = app.current_view();
        assert_eq!(v.hidden, vec![5]);
        let mut app2 = app_n(10);
        app2.apply_view(v);
        assert!(app2.hidden_lines.contains(&5));
        assert!(app2.pos_of_line(5).is_none());
    }

    #[test]
    fn col_hide_char_and_whitespace() {
        let mut app = app_n(1);
        app.hide_cols_text = "2,4".into();
        app.col_sep_text = ",".into();
        assert_eq!(app.apply_col_hide("a,b,c,d"), "a,…,c,…");
        assert_eq!(app.apply_col_hide("a,b"), "a,…"); // col 4 absent → ignored
        app.col_sep_text = "space".into();
        app.hide_cols_text = "1,3".into();
        assert_eq!(app.apply_col_hide("2026 INFO foo bar"), "… INFO … bar");
        app.hide_cols_text = "".into();
        assert_eq!(app.apply_col_hide("a,b,c"), "a,b,c"); // no columns → unchanged
    }

    #[test]
    fn col_hide_round_trips_through_view() {
        use crate::view::state::parse_view;
        let mut app = app_n(5);
        app.hide_cols_text = "1,3".into();
        app.col_sep_text = "|".into();
        let v = app.current_view();
        assert_eq!(v.hide_cols, "1,3");
        assert_eq!(v.col_sep, "|");
        let mut app2 = app_n(5);
        app2.apply_view(parse_view(&serialize_view(&v)));
        assert_eq!(app2.hide_cols_text, "1,3");
        assert_eq!(app2.col_sep_text, "|");
    }

    #[test]
    fn menu_edits_hide_columns_field_inline() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_n(5); // no filters/highlights → field rows at sel 0,1
        app.overlay = Overlay::Menu;
        app.menu_sel = 0;
        let k = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        app.handle_key(k(KeyCode::Down)); // move to the Hide Columns field
        assert_eq!(app.sel_field(), Some(MenuField::Columns));
        app.handle_key(k(KeyCode::Enter)); // begin editing in place
        assert_eq!(app.menu_edit, Some(MenuField::Columns));
        for c in "2,4".chars() {
            app.handle_key(k(KeyCode::Char(c)));
        }
        app.handle_key(k(KeyCode::Enter)); // commit
        assert!(app.menu_edit.is_none());
        assert_eq!(app.hide_cols_text, "2,4");
    }

    #[test]
    fn menu_edits_bookmarks_field_inline() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_n(20); // no filters/highlights → fields at sel 0,1,2
        app.overlay = Overlay::Menu;
        app.menu_sel = 0;
        let k = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        app.handle_key(k(KeyCode::Down)); // → Hide Columns
        app.handle_key(k(KeyCode::Down)); // → Bookmarks
        assert_eq!(app.sel_field(), Some(MenuField::Bookmarks));
        app.handle_key(k(KeyCode::Enter));
        for c in "7,4,999,4".chars() {
            app.handle_key(k(KeyCode::Char(c)));
        }
        app.handle_key(k(KeyCode::Enter)); // commit
        // sorted, deduped, and 999 (> 20 lines) clamped away
        assert_eq!(app.bookmarks.iter().copied().collect::<Vec<_>>(), vec![4, 7]);
    }

    #[test]
    fn bad_regex_sets_error() {
        let mut app = app_n(5);
        app.search_regex = true;
        app.run_search("(");
        assert_eq!(app.search_error, Some("bad regex"));
        assert!(app.matches.is_empty());
    }

    #[test]
    fn history_recall_cycles() {
        let mut app = app_n(5);
        app.run_search("aaa");
        app.run_search("bbb");
        app.mode = Mode::Search;
        app.history_recall(1);
        assert_eq!(app.input, "bbb"); // newest first
        app.history_recall(1);
        assert_eq!(app.input, "aaa");
        app.history_recall(-1);
        assert_eq!(app.input, "bbb");
    }

    fn displayed_lines(app: &App) -> Vec<usize> {
        app.displayed
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|it| match it {
                DisplayedItem::Line(n) => Some(*n),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn filter_builds_displayed_with_gaps() {
        let mut app = app_from(&["aaa", "bbb match", "ccc", "ddd match", "eee"]);
        app.add_filter("match");
        assert!(app.displayed.is_some());
        assert_eq!(displayed_lines(&app), vec![2, 4]);
        // AND->OR toggle keeps the (single-term) result here.
        app.toggle_filter_mode();
        assert_eq!(displayed_lines(&app), vec![2, 4]);
    }

    #[test]
    fn bookmark_toggle_and_nav() {
        let mut app = app_n(10);
        app.cursor = 2; // line 3
        app.toggle_bookmark();
        assert!(app.bookmarks.contains(&3));
        app.cursor = 6; // line 7
        app.toggle_bookmark();
        app.cursor = 0;
        app.goto_bookmark(true);
        assert_eq!(app.current_line(), Some(3));
        app.goto_bookmark(true);
        assert_eq!(app.current_line(), Some(7));
        app.cursor = 2;
        app.toggle_bookmark();
        assert!(!app.bookmarks.contains(&3));
    }

    #[test]
    fn gap_reveal_expands_visible() {
        let mut app = app_from(&["a", "b M", "c", "d", "e", "f", "g", "h M", "i", "j"]);
        app.add_filter("M"); // matches lines 2 and 8
        let d = app.displayed.as_ref().unwrap().clone();
        let gap_pos = d
            .iter()
            .position(|it| matches!(it, DisplayedItem::Gap { start: 3, .. }))
            .unwrap();
        app.cursor = gap_pos;
        app.gap_action(true);
        for l in 3..=7 {
            assert!(app.visible_lines.contains(&l), "line {l} should be revealed");
        }
    }

    #[test]
    fn apply_view_restores_filters_and_selected() {
        use crate::view::state::parse_view;
        let mut app = app_from(&["error a", "ok b", "error c", "ok d", "error e"]);
        // selected line 3 is in the filtered view → land on it.
        app.apply_view(parse_view("filters=error&selected=3"));
        assert_eq!(app.filters.len(), 1);
        assert!(app.displayed.is_some());
        assert_eq!(app.current_line(), Some(3));

        // selected line 2 is filtered out → revealed, then focused.
        let mut app2 = app_from(&["error a", "ok b", "error c"]);
        app2.apply_view(parse_view("filters=error&selected=2"));
        assert!(app2.visible_lines.contains(&2));
        assert_eq!(app2.current_line(), Some(2));
    }

    #[test]
    fn time_brush_filters_to_bucket() {
        let mut app = app_from(&[
            "2026-06-20T00:00:00Z a",
            "2026-06-20T00:00:00Z b",
            "no timestamp",
            "2026-06-20T05:00:00Z c",
        ]);
        app.toggle_hist();
        assert!(app.show_hist);
        app.ensure_hist(4);
        app.hist_cursor = 0;
        app.hist_anchor = 0;
        app.apply_time_brush();
        assert!(app.time_range.is_some());
        let lines = displayed_lines(&app);
        assert!(lines.contains(&1) && lines.contains(&2));
        assert!(!lines.contains(&4)); // 5h line is in a later bucket
    }

    #[test]
    fn duplicate_filter_or_highlight_is_dropped() {
        let mut app = app_from(&["error one", "ok two", "error three"]);
        app.add_filter("error");
        app.add_filter("error"); // same string → skipped, hint set
        assert_eq!(app.filters.len(), 1);
        assert!(app.hint.is_some());
        app.add_highlight("foo");
        app.add_highlight("foo");
        assert_eq!(app.highlights.len(), 1);
    }

    #[test]
    fn menu_counts_defer_and_drain() {
        let mut app = app_from(&["error one", "ok two", "error three", "warn four"]);
        app.add_filter("error");
        app.add_highlight("o");
        // add_* does NOT scan; slots start pending so the first frame is fast.
        assert_eq!(app.filter_counts, vec![Count::Pending]);
        assert_eq!(app.highlight_counts, vec![Count::Pending]);
        assert!(app.has_pending_counts());

        // Drain one at a time (the run-loop calls this between frames).
        app.compute_next_pending_count();
        assert_eq!(app.filter_counts, vec![Count::Value(2)]); // "error" on lines 1 & 3
        app.compute_next_pending_count();
        assert_eq!(app.highlight_counts, vec![Count::Value(4)]); // "o" on all 4 lines (CI)
        assert!(!app.has_pending_counts());

        // Editing a term re-marks it pending; the run loop will refill it.
        app.filters[0].regex = true;
        app.filter_counts[0] = Count::Pending;
        assert!(app.has_pending_counts());
    }

    #[test]
    fn menu_exports_view_and_shell() {
        let mut app = app_from(&["error one", "ok two", "error three"]);
        app.add_filter("error");
        app.export_view();
        match &app.overlay {
            Overlay::Text { body, .. } => assert!(body[0].contains("filters=error"), "got {body:?}"),
            _ => panic!("expected a text overlay"),
        }
        app.export_shell();
        match &app.overlay {
            Overlay::Text { body, .. } => {
                assert!(body.join("\n").contains("grep -iF 'error'"), "got {body:?}")
            }
            _ => panic!("expected a text overlay"),
        }
    }

    #[test]
    fn enter_opens_the_right_overlay() {
        let long = "x".repeat(500);
        let mut app = app_from(&["short", &long, "{\"a\":1}"]);
        app.width = 80;
        app.cursor = 0; // short line fits → no overlay
        app.enter_action();
        assert!(matches!(app.overlay, Overlay::None));
        app.cursor = 1; // long line → wrapped view
        app.enter_action();
        assert!(matches!(app.overlay, Overlay::Line(2)));
        app.overlay = Overlay::None;
        app.cursor = 2; // JSON line → JSON view
        app.enter_action();
        assert!(matches!(app.overlay, Overlay::Json(3)));
    }

    #[test]
    fn recalled_term_is_editable_in_place() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        let mut app = app_n(5);
        app.run_search("error");
        app.mode = Mode::Highlight;
        app.history_recall(1);
        assert_eq!(app.input, "error");
        assert_eq!(app.input_cursor, 5); // caret at end after recall

        // Home + insert at the front.
        app.handle_input_key(k(KeyCode::Home), false);
        app.handle_input_key(k(KeyCode::Char('X')), false);
        assert_eq!(app.input, "Xerror");
        assert_eq!(app.input_cursor, 1);

        // Move right and delete a char in the middle (Backspace deletes before caret).
        app.handle_input_key(k(KeyCode::Right), false); // caret=2 (before 'r')
        app.handle_input_key(k(KeyCode::Backspace), false); // remove 'e' at index 1
        assert_eq!(app.input, "Xrror");

        // Delete key removes the char under the caret.
        app.handle_input_key(k(KeyCode::Delete), false);
        assert_eq!(app.input, "Xror");
    }

    #[test]
    fn inline_regex_toggle_in_highlight_and_filter() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let typ = |app: &mut App, s: &str| {
            for c in s.chars() {
                app.handle_input_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), false);
            }
        };
        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        let mut app = app_n(5);
        app.mode = Mode::Highlight;
        app.reset_input();
        app.handle_input_key(ctrl_r, true); // ^R → regex on
        typ(&mut app, "[a-z]+");
        app.handle_input_key(enter, false);
        assert!(app.highlights[0].regex);

        app.mode = Mode::Filter;
        app.reset_input();
        app.handle_input_key(ctrl_r, true);
        typ(&mut app, "err");
        app.handle_input_key(enter, false);
        assert!(app.filters[0].regex);
    }

    #[test]
    fn history_is_shared_across_input_modes() {
        let mut app = app_from(&["error a", "ok b"]);
        app.run_search("srch");
        app.add_filter("filt");
        app.add_highlight("hilite");
        // Any input mode recalls the shared list, newest first.
        app.mode = Mode::Highlight;
        app.history_recall(1);
        assert_eq!(app.input, "hilite");
        app.history_recall(1);
        assert_eq!(app.input, "filt");
        app.history_recall(1);
        assert_eq!(app.input, "srch");
    }

    #[test]
    fn highlights_add_with_palette() {
        let mut app = app_n(5);
        app.add_highlight("foo");
        app.add_highlight("bar");
        assert_eq!(app.highlights.len(), 2);
        assert_eq!(app.highlights[0].color, HIGHLIGHT_PALETTE[0]);
        assert_eq!(app.highlights[1].color, HIGHLIGHT_PALETTE[1]);
    }
}
