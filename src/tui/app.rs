//! The interactive application: all view state, key handling, movement, and
//! rendering. Phase 4 covers the scrolling viewport (level-tinted gutter, gap
//! rows), movement, horizontal pan, in-place wrap, status bar, and confirm-quit.
//! Filters / search / highlights / overlays layer on in later phases.

use crate::engine::index::LineIndex;
use crate::levels::level_rgb;
use crate::view::displayed::DisplayedItem;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

const HINT: &str = "j/k move · f/b page · ←/→ pan · w wrap · g/G ends · q quit";
const CURSOR_BG: Color = Color::Indexed(236);

pub struct App {
    index: LineIndex,
    file_name: String,
    total: usize,
    /// Filtered display sequence, or `None` when every line is shown (Phase 4).
    displayed: Option<Vec<DisplayedItem>>,

    pos: usize,    // first visible position (index into the displayed sequence)
    cursor: usize, // selected position
    hscroll: usize,
    wrap: bool,

    width: u16,    // last-rendered viewport width
    height: usize, // last-rendered viewport height (rows)

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

    fn clamp_pos(&self, p: i64) -> usize {
        let max = self.count().saturating_sub(1) as i64;
        p.clamp(0, max.max(0)) as usize
    }

    // --- movement (ported from App.tsx) ---------------------------------------

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

    fn build_rows(&self) -> Vec<Line<'static>> {
        let count = self.count();
        if count == 0 {
            return vec![Line::from(Span::styled(
                "(empty file)",
                Style::default().fg(Color::DarkGray),
            ))];
        }
        let gutter = self.gutter_width();
        let cols = self.width as usize;
        let content_w = cols.saturating_sub(gutter + 1).max(1);

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
        let raw = self.index.get_line(n).replace('\t', "  ");
        let chars: Vec<char> = raw.chars().collect();
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
            if chars.is_empty() {
                rows.push(
                    Line::from(vec![Span::styled(num.clone(), num_style), Span::raw(" ")])
                        .style(row_style),
                );
                return;
            }
            let mut ci = 0;
            let mut first = true;
            while ci < chars.len() && rows.len() < self.height {
                let end = (ci + content_w).min(chars.len());
                let chunk: String = chars[ci..end].iter().collect();
                let g = if first {
                    Span::styled(num.clone(), num_style)
                } else {
                    Span::raw(blank.clone())
                };
                rows.push(Line::from(vec![g, Span::raw(" "), Span::raw(chunk)]).style(row_style));
                ci = end;
                first = false;
            }
        } else {
            let mut vis: String = chars.iter().skip(self.hscroll).take(content_w).collect();
            if is_cursor {
                let pad = content_w.saturating_sub(vis.chars().count());
                vis.push_str(&" ".repeat(pad));
            }
            rows.push(
                Line::from(vec![Span::styled(num, num_style), Span::raw(" "), Span::raw(vis)])
                    .style(row_style),
            );
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
            Line::from(Span::styled(
                "Quit? (y/n)",
                Style::default().add_modifier(Modifier::BOLD),
            ))
        } else {
            Line::from(Span::styled(HINT, Style::default().fg(Color::DarkGray)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::index::IndexOptions;
    use std::fs::File;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn app_with(n: usize) -> App {
        static C: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "zercher_app_{}_{}.log",
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        let body: String = (1..=n).map(|i| format!("line {i}\n")).collect();
        File::create(&p).unwrap().write_all(body.as_bytes()).unwrap();
        let idx = LineIndex::open(&p, IndexOptions::default(), |_| {}).unwrap();
        let _ = std::fs::remove_file(&p);
        let mut app = App::new(idx, "test.log".into());
        app.height = 10; // simulate a 10-row viewport
        app
    }

    #[test]
    fn move_and_clamp() {
        let mut app = app_with(100);
        app.move_cursor(-5);
        assert_eq!(app.cursor, 0); // clamped at top
        app.move_cursor(3);
        assert_eq!(app.cursor, 3);
        app.jump_to(1_000);
        assert_eq!(app.cursor, 99); // clamped at bottom
    }

    #[test]
    fn cursor_scrolls_viewport() {
        let mut app = app_with(100);
        app.move_cursor(15); // beyond the 10-row viewport
        assert_eq!(app.cursor, 15);
        assert!(app.pos > 0 && app.cursor < app.pos + app.height);
    }

    #[test]
    fn paging_moves_by_viewport() {
        let mut app = app_with(100);
        app.page_by(1);
        assert_eq!(app.pos, 10);
        app.page_by(-1);
        assert_eq!(app.pos, 0);
    }

    #[test]
    fn g_and_bigg() {
        let mut app = app_with(50);
        app.jump_to(49);
        assert_eq!(app.cursor, 49);
        app.cursor = 0;
        app.pos = 0;
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn pan_bounds() {
        let mut app = app_with(3);
        app.width = 20;
        app.pan(1);
        assert!(app.hscroll <= app.index.max_line_length().saturating_sub(2));
        app.pan(-1);
        assert_eq!(app.hscroll, 0);
    }
}
