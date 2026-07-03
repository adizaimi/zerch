//! The interactive application: all view state, key handling, movement, search,
//! highlighting, and rendering. Filters / bookmarks / gap-reveal and overlays
//! layer on in the following commits/phases.

use crate::engine::index::LineIndex;
use crate::engine::search::{search, Op, Scope, SearchTerm};
use crate::levels::{hex_rgb, level_rgb};
use crate::view::displayed::DisplayedItem;
use crate::view::highlight::{build_spans, match_ranges, HighlightSpec};
use crate::view::state::HighlightEntry;
use crate::view::HIGHLIGHT_PALETTE;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

const HINT: &str = "j/k move · f/b page · ←/→ pan · w wrap · /search n/N · h hl · g/G · q quit";
const CURSOR_BG: Color = Color::Indexed(236);
const SEARCH_HISTORY_MAX: usize = 50;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Search,
    Highlight,
}

pub struct App {
    index: LineIndex,
    file_name: String,
    total: usize,
    displayed: Option<Vec<DisplayedItem>>,

    pos: usize,
    cursor: usize,
    hscroll: usize,
    wrap: bool,

    width: u16,
    height: usize,

    mode: Mode,
    input: String,

    search_term: String,
    search_regex: bool,
    search_case: bool,
    search_error: Option<&'static str>,
    matches: Vec<usize>,
    match_idx: usize,
    search_history: Vec<String>,
    search_hist_idx: i64, // -1 = not browsing

    highlights: Vec<HighlightEntry>,

    confirm_quit: bool,
    pub should_quit: bool,
}

impl App {
    pub fn new(index: LineIndex, file_name: String) -> Self {
        let total = index.total_lines();
        App {
            index,
            file_name,
            total,
            displayed: None,
            pos: 0,
            cursor: 0,
            hscroll: 0,
            wrap: false,
            width: 80,
            height: 24,
            mode: Mode::Normal,
            input: String::new(),
            search_term: String::new(),
            search_regex: false,
            search_case: false,
            search_error: None,
            matches: Vec::new(),
            match_idx: 0,
            search_history: Vec::new(),
            search_hist_idx: -1,
            highlights: Vec::new(),
            confirm_quit: false,
            should_quit: false,
        }
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
        match &self.displayed {
            Some(_) => self.pos_of_line(ln).is_some(),
            None => ln >= 1 && ln <= self.total,
        }
    }

    fn pos_of_line(&self, ln: usize) -> Option<usize> {
        match &self.displayed {
            Some(d) => d
                .iter()
                .position(|it| matches!(it, DisplayedItem::Line(x) if *x == ln)),
            None => (ln >= 1 && ln <= self.total).then(|| ln - 1),
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
            self.pos = self.clamp_pos(nc as i64);
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

    fn pan(&mut self, dir: i64) {
        if self.wrap {
            return;
        }
        let step = (self.width as usize / 2).max(8);
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

    fn matches_in_view(&self) -> Vec<usize> {
        if self.search_term.is_empty() {
            return Vec::new();
        }
        self.matches.iter().copied().filter(|&l| self.in_view(l)).collect()
    }

    fn run_search(&mut self, term: &str) {
        self.mode = Mode::Normal;
        if term.is_empty() {
            return;
        }
        push_history(&mut self.search_history, term);
        self.search_term = term.to_string();
        self.match_idx = 0;
        if self.search_regex && bad_regex(term) {
            self.search_error = Some("bad regex");
            self.matches.clear();
            return;
        }
        self.search_error = None;
        let stack = [SearchTerm::new(term, Op::And)
            .case(self.search_case)
            .rx(self.search_regex)];
        match search(&self.index, &stack, &Scope::default()) {
            Ok(res) => {
                self.matches = res.matching_lines;
                if let Some(&first) = self.matches.iter().find(|&&l| self.in_view(l)) {
                    if let Some(p) = self.pos_of_line(first) {
                        self.jump_to(p as i64);
                    }
                    self.scroll_to_col(first);
                }
            }
            Err(_) => {
                self.matches.clear();
                self.search_error = Some("error");
            }
        }
    }

    fn goto_match(&mut self, idx: i64) {
        let miv = self.matches_in_view();
        if miv.is_empty() {
            return;
        }
        let len = miv.len() as i64;
        let wrapped = ((idx % len) + len) % len;
        self.match_idx = wrapped as usize;
        let line = miv[wrapped as usize];
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
        let color = HIGHLIGHT_PALETTE[self.highlights.len() % HIGHLIGHT_PALETTE.len()].to_string();
        self.highlights.push(HighlightEntry {
            term: term.to_string(),
            color,
            enabled: true,
            case_sensitive: false,
            regex: false,
        });
    }

    // --- key handling ---------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            self.should_quit = true;
            return;
        }
        if self.confirm_quit {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.should_quit = true,
                _ => self.confirm_quit = false,
            }
            return;
        }
        if self.mode != Mode::Normal {
            self.handle_input_key(key, ctrl);
            return;
        }
        self.handle_normal_key(key, ctrl);
    }

    fn handle_input_key(&mut self, key: KeyEvent, ctrl: bool) {
        if ctrl && self.mode == Mode::Search {
            match key.code {
                KeyCode::Char('r') => self.search_regex = !self.search_regex,
                KeyCode::Char('a') => self.search_case = !self.search_case,
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Enter => {
                let term = self.input.trim().to_string();
                match self.mode {
                    Mode::Search => self.run_search(&term),
                    Mode::Highlight => {
                        self.add_highlight(&term);
                        self.mode = Mode::Normal;
                    }
                    Mode::Normal => {}
                }
                self.input.clear();
                self.search_hist_idx = -1;
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
                self.search_hist_idx = -1;
            }
            KeyCode::Backspace => {
                if self.input.is_empty() {
                    self.mode = Mode::Normal;
                } else {
                    self.input.pop();
                }
                self.search_hist_idx = -1;
            }
            KeyCode::Up if self.mode == Mode::Search => self.history_recall(1),
            KeyCode::Down if self.mode == Mode::Search => self.history_recall(-1),
            KeyCode::Char(c) => {
                self.input.push(c);
                self.search_hist_idx = -1;
            }
            _ => {}
        }
    }

    fn history_recall(&mut self, dir: i64) {
        let len = self.search_history.len() as i64;
        if len == 0 {
            return;
        }
        if dir > 0 {
            let ni = (self.search_hist_idx + 1).min(len - 1);
            self.search_hist_idx = ni;
            self.input = self.search_history[ni as usize].clone();
        } else if self.search_hist_idx <= 0 {
            self.search_hist_idx = -1;
            self.input.clear();
        } else {
            let ni = self.search_hist_idx - 1;
            self.search_hist_idx = ni;
            self.input = self.search_history[ni as usize].clone();
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent, ctrl: bool) {
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
            KeyCode::Left => self.pan(-1),
            KeyCode::Right => self.pan(1),
            KeyCode::Char('0') => self.hscroll = 0,
            KeyCode::Char('w') => {
                self.wrap = !self.wrap;
                self.hscroll = 0;
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.input.clear();
                self.search_hist_idx = -1;
            }
            KeyCode::Char('h') => {
                self.mode = Mode::Highlight;
                self.input.clear();
            }
            KeyCode::Char('n') => self.goto_match(self.match_idx as i64 + 1),
            KeyCode::Char('N') => self.goto_match(self.match_idx as i64 - 1),
            KeyCode::Esc => {
                self.search_term.clear();
                self.matches.clear();
                self.search_error = None;
            }
            _ => {}
        }
    }

    // --- rendering ------------------------------------------------------------

    pub fn render(&mut self, f: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());
        self.width = chunks[0].width;
        self.height = chunks[0].height as usize;

        f.render_widget(Paragraph::new(self.build_rows()), chunks[0]);
        f.render_widget(
            Paragraph::new(self.status_line()).style(Style::default().add_modifier(Modifier::REVERSED)),
            chunks[1],
        );
        f.render_widget(Paragraph::new(self.prompt_line()), chunks[2]);
    }

    fn gutter_width(&self) -> usize {
        self.total.to_string().len().max(2)
    }

    fn enabled_highlight_specs(&self) -> Vec<HighlightSpec> {
        self.highlights
            .iter()
            .filter(|h| h.enabled && !h.term.is_empty())
            .map(|h| HighlightSpec {
                term: h.term.clone(),
                color: h.color.clone(),
                case_sensitive: h.case_sensitive,
                regex: h.regex,
            })
            .collect()
    }

    // Per-character styled content for one line (highlights + search overlay).
    fn style_line(&self, n: usize) -> Vec<(char, Style)> {
        let raw = self.index.get_line(n).replace('\t', "  ");
        let specs = self.enabled_highlight_specs();
        let spans = build_spans(
            &raw,
            &specs,
            &self.search_term,
            self.search_case,
            self.search_regex,
        );
        let mut out: Vec<(char, Style)> = Vec::with_capacity(raw.len());
        for s in spans {
            let style = if s.search {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else if let Some(c) = s.color {
                let (r, g, b) = hex_rgb(&c);
                Style::default().fg(Color::Rgb(r, g, b))
            } else {
                Style::default()
            };
            for ch in s.text.chars() {
                out.push((ch, style));
            }
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
            match self.item_at(p) {
                DisplayedItem::Line(n) => {
                    self.push_line_rows(&mut rows, n, is_cursor, gutter, content_w);
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
        gutter: usize,
        content_w: usize,
    ) {
        let styled = self.style_line(n);
        let num_style = match level_rgb(self.index.level_of(n)) {
            Some((r, g, b)) => Style::default().fg(Color::Rgb(r, g, b)),
            None => Style::default().fg(Color::DarkGray),
        };
        let num = format!("{n:>gutter$}");
        let blank = " ".repeat(gutter);
        let row_style = if is_cursor {
            Style::default().bg(CURSOR_BG)
        } else {
            Style::default()
        };

        if self.wrap {
            if styled.is_empty() {
                rows.push(
                    Line::from(vec![Span::styled(num, num_style), Span::raw(" ")]).style(row_style),
                );
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
                rows.push(Line::from(spans).style(row_style));
                ci = end;
                first = false;
            }
        } else {
            let start = self.hscroll.min(styled.len());
            let end = (start + content_w).min(styled.len());
            let mut spans = vec![Span::styled(num, num_style), Span::raw(" ")];
            spans.extend(group_spans(&styled[start..end]));
            if is_cursor {
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
            let miv = self.matches_in_view().len();
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
        Line::from(s)
    }

    fn prompt_line(&self) -> Line<'static> {
        if self.confirm_quit {
            return Line::from(Span::styled(
                "Quit? (y/n)",
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        match self.mode {
            Mode::Search => {
                let flags = format!(
                    "  [{} {}] ^R rx · ^A case · ↑↓ hist",
                    if self.search_regex { ".*" } else { "ab" },
                    if self.search_case { "Aa" } else { "aa" }
                );
                Line::from(vec![
                    Span::raw(format!("/{}", self.input)),
                    Span::styled("▏", Style::default().add_modifier(Modifier::REVERSED)),
                    Span::styled(flags, Style::default().fg(Color::DarkGray)),
                ])
            }
            Mode::Highlight => Line::from(vec![
                Span::raw(format!("highlight: {}", self.input)),
                Span::styled("▏", Style::default().add_modifier(Modifier::REVERSED)),
            ]),
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

fn bad_regex(pattern: &str) -> bool {
    regex::Regex::new(pattern).is_err()
}

fn push_history(history: &mut Vec<String>, term: &str) {
    if term.is_empty() {
        return;
    }
    history.retain(|t| t != term);
    history.insert(0, term.to_string());
    history.truncate(SEARCH_HISTORY_MAX);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::index::IndexOptions;
    use std::fs::File;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn app_from(lines: &[&str]) -> App {
        static C: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "zercher_app_{}_{}.log",
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
    fn search_finds_and_jumps() {
        let mut app = app_from(&["alpha", "beta ERROR", "gamma", "delta ERROR"]);
        app.run_search("ERROR");
        assert_eq!(app.matches, vec![2, 4]);
        assert_eq!(app.cursor, 1); // jumped to first match (line 2 → pos 1)
        app.goto_match(app.match_idx as i64 + 1);
        assert_eq!(app.cursor, 3); // line 4
        app.goto_match(app.match_idx as i64 + 1);
        assert_eq!(app.cursor, 1); // wrapped back to line 2
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
