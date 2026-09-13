//! Render one log line to HTML for the web UI. Web-local span assembly (not
//! `view::highlight::build_spans`, which knows only two lanes): it composes the
//! background lanes at zercher's priority — search > highlight > filter-match — plus
//! an optional syntax-colour foreground. The selected-text (teal) lane is applied
//! client-side, so it is absent here.
//!
//! All line text is untrusted, so every character is HTML-escaped.

use crate::view::highlight::{match_ranges, syntax_ranges, SyntaxTok};
use crate::view::state::ViewState;

/// Foreground colour for a syntax token — zercher's JSON palette (LineRow.css).
fn syntax_hex(t: SyntaxTok) -> &'static str {
    match t {
        SyntaxTok::Key => "#8e44ad",
        SyntaxTok::Str => "#16825d",
        SyntaxTok::Num => "#1c75d8",
        SyntaxTok::Bool => "#c0392b",
        SyntaxTok::Null => "#7f8c8d",
    }
}

// Background lane for a character, lowest to highest priority.
#[derive(Clone, PartialEq)]
enum Bg {
    None,
    Filter,
    Highlight(String),
    Search,
}

/// Escape `&<>"'` so untrusted log text is inert in HTML (text + attribute contexts).
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Inner HTML of a line's `.line-content`: escaped text in `<span>`s carrying the
/// background lane (class `s`/`f` or inline highlight colour) and optional syntax
/// foreground colour.
pub fn line_html(view: &ViewState, text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n == 0 {
        return String::new();
    }

    // Foreground: syntax colours (only when the toggle is on).
    let mut fg: Vec<Option<&'static str>> = vec![None; n];
    if view.syntax {
        for (s, e, tok) in syntax_ranges(text) {
            let c = syntax_hex(tok);
            for slot in fg.iter_mut().take(e.min(n)).skip(s.min(n)) {
                *slot = Some(c);
            }
        }
    }

    // Background lanes, applied lowest priority first so higher lanes overwrite.
    let mut bg: Vec<Bg> = vec![Bg::None; n];

    // Filter-match tint (enabled, non-negated filters only).
    for f in &view.filters {
        if !f.enabled || f.negate || f.term.is_empty() {
            continue;
        }
        for (s, e) in match_ranges(text, &f.term, f.case_sensitive, f.regex) {
            for slot in bg.iter_mut().take(e.min(n)).skip(s.min(n)) {
                *slot = Bg::Filter;
            }
        }
    }
    // Highlights (earlier highlight wins on overlap; overwrites the lower filter lane).
    for h in &view.highlights {
        if !h.enabled || h.term.is_empty() {
            continue;
        }
        for (s, e) in match_ranges(text, &h.term, h.case_sensitive, h.regex) {
            for slot in bg.iter_mut().take(e.min(n)).skip(s.min(n)) {
                if !matches!(slot, Bg::Highlight(_)) {
                    *slot = Bg::Highlight(h.color.clone());
                }
            }
        }
    }
    // Search — highest priority.
    for (s, e) in match_ranges(text, &view.search, view.search_case, view.search_regex) {
        for slot in bg.iter_mut().take(e.min(n)).skip(s.min(n)) {
            *slot = Bg::Search;
        }
    }

    // Coalesce runs of identical (fg, bg) into spans.
    let mut out = String::new();
    let mut i = 0;
    while i < n {
        let start = i;
        let cur_fg = fg[i];
        let cur_bg = bg[i].clone();
        i += 1;
        while i < n && fg[i] == cur_fg && bg[i] == cur_bg {
            i += 1;
        }
        let seg: String = chars[start..i].iter().collect();
        push_span(&mut out, &seg, cur_fg, &cur_bg);
    }
    out
}

fn push_span(out: &mut String, seg: &str, fg: Option<&str>, bg: &Bg) {
    let escaped = escape_html(seg);
    let mut class = "";
    let mut style = String::new();
    match bg {
        Bg::None => {}
        Bg::Filter => class = "f",
        Bg::Search => class = "s",
        Bg::Highlight(c) => {
            class = "h";
            style.push_str("background-color:");
            style.push_str(c);
            style.push(';');
        }
    }
    if let Some(c) = fg {
        style.push_str("color:");
        style.push_str(c);
        style.push(';');
    }
    if class.is_empty() && style.is_empty() {
        out.push_str(&escaped);
        return;
    }
    out.push_str("<span");
    if !class.is_empty() {
        out.push_str(" class=\"");
        out.push_str(class);
        out.push('"');
    }
    if !style.is_empty() {
        out.push_str(" style=\"");
        out.push_str(&style);
        out.push('"');
    }
    out.push('>');
    out.push_str(&escaped);
    out.push_str("</span>");
}

/// Gutter left-edge tint per canonical log level (levels.rs codes 0..=6),
/// matching zercher's `levels.ts`.
pub fn level_hex(level: u8) -> &'static str {
    match level {
        1 => "#9e9e9e", // TRACE
        2 => "#78909c", // DEBUG
        3 => "#4caf50", // INFO
        4 => "#ffa726", // WARN
        5 => "#ef5350", // ERROR
        6 => "#c2185b", // FATAL
        _ => "#90a4ae", // none / unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::state::{FilterEntry, HighlightEntry, ViewState};

    fn v() -> ViewState {
        ViewState::default()
    }

    #[test]
    fn plain_text_has_no_spans() {
        assert_eq!(line_html(&v(), "hello world"), "hello world");
    }

    #[test]
    fn escapes_untrusted_text() {
        assert_eq!(
            line_html(&v(), "a <script>&\"'"),
            "a &lt;script&gt;&amp;&quot;&#39;"
        );
    }

    #[test]
    fn search_lane_wraps_matches() {
        let view = ViewState { search: "err".into(), ..v() };
        assert_eq!(line_html(&view, "an err here"), "an <span class=\"s\">err</span> here");
    }

    #[test]
    fn highlight_uses_inline_background_color() {
        let view = ViewState {
            highlights: vec![HighlightEntry {
                term: "ok".into(),
                color: "#81c784".into(),
                enabled: true,
                case_sensitive: false,
                regex: false,
            }],
            ..v()
        };
        assert_eq!(
            line_html(&view, "ok!"),
            "<span class=\"h\" style=\"background-color:#81c784;\">ok</span>!"
        );
    }

    #[test]
    fn search_wins_over_highlight_and_filter() {
        let view = ViewState {
            search: "abc".into(),
            highlights: vec![HighlightEntry {
                term: "abc".into(),
                color: "#111".into(),
                enabled: true,
                case_sensitive: false,
                regex: false,
            }],
            ..v()
        };
        assert_eq!(line_html(&view, "abc"), "<span class=\"s\">abc</span>");
    }

    #[test]
    fn negated_filter_does_not_tint() {
        let view = ViewState {
            filters: vec![FilterEntry {
                term: "no".into(),
                enabled: true,
                case_sensitive: false,
                regex: false,
                negate: true,
            }],
            ..v()
        };
        assert_eq!(line_html(&view, "no"), "no");
    }

    #[test]
    fn enabled_filter_tints_light_blue() {
        let view = ViewState {
            filters: vec![FilterEntry {
                term: "yes".into(),
                enabled: true,
                case_sensitive: false,
                regex: false,
                negate: false,
            }],
            ..v()
        };
        assert_eq!(line_html(&view, "yes!"), "<span class=\"f\">yes</span>!");
    }
}
