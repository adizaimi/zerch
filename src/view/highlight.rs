//! Span-building for the line renderer: split a line into coloured spans given the
//! highlight terms and active search term. Search matches win over highlights, and
//! earlier highlights win over later ones on overlap. `color_json_line` adds a JSON
//! syntax-colour base layer. Ported from `src/tui/highlight.ts`.
//!
//! Offsets are computed in `char` units (not UTF-16 like JS); identical for the
//! BMP text that dominates logs. Case-insensitive literal matching folds ASCII,
//! matching the search engine.

use crate::view::jsonline::{tokenize_json, JsonTokenType};
use regex::{Regex, RegexBuilder};
use std::sync::LazyLock;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HighlightSpec {
    pub term: String,
    pub color: String,
    pub case_sensitive: bool,
    pub regex: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Span {
    pub text: String,
    pub color: Option<String>, // highlight foreground (None for plain / search)
    pub search: bool,          // part of a search-term match (black-on-yellow)
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JsonSpan {
    pub text: String,
    pub color: Option<String>,
    pub search: bool,
    pub hl: bool, // part of a highlight match (rendered bold)
}

/// Lightweight syntax token for the optional per-line syntax colouring.
/// Recognizes logfmt `key=value` pairs and whole-line JSON.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyntaxTok {
    Key,
    Str,
    Num,
    Bool,
    Null,
}

// logfmt pair: identifier `=` (quoted-string | run of non-separator chars).
// The unquoted value stops at whitespace and at delimiters commonly used to
// separate pairs (`,` `;`) or to close a wrapping context (`)`, `]`, `}`), so
// e.g. `a=1,b=2` tokenizes as two pairs instead of `a` = `1,b=2`.
static LOGFMT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"([A-Za-z_][A-Za-z0-9_.\-]*)=("(?:[^"\\]|\\.)*"|[^\s,;)\]}]+)"#).unwrap()
});

// Char-index of a byte offset inside `text`.
fn char_at(text: &str, byte: usize) -> usize {
    text[..byte].chars().count()
}

/// Char-index ranges `[start, end)` of syntax tokens in `text`. Whole-line JSON
/// (trimmed line starts with `{`/`[` and parses) uses the JSON tokenizer;
/// everything else uses a loose logfmt scan. Empty for plain-prose lines.
pub fn syntax_ranges(text: &str) -> Vec<(usize, usize, SyntaxTok)> {
    let mut out = Vec::new();
    let trimmed = text.trim_start();
    let prefix_bytes = text.len() - trimmed.len();
    let json_ok = (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok();

    if json_ok {
        let mut off = char_at(text, prefix_bytes);
        for t in tokenize_json(trimmed) {
            let len = t.text.chars().count();
            let kind = match t.kind {
                JsonTokenType::Key => Some(SyntaxTok::Key),
                JsonTokenType::String => Some(SyntaxTok::Str),
                JsonTokenType::Number => Some(SyntaxTok::Num),
                JsonTokenType::Boolean => Some(SyntaxTok::Bool),
                JsonTokenType::Null => Some(SyntaxTok::Null),
                _ => None,
            };
            if let Some(k) = kind {
                out.push((off, off + len, k));
            }
            off += len;
        }
        return out;
    }

    for c in LOGFMT_RE.captures_iter(text) {
        let key = c.get(1).unwrap();
        let val = c.get(2).unwrap();
        out.push((char_at(text, key.start()), char_at(text, key.end()), SyntaxTok::Key));
        let v = val.as_str();
        let kind = if v.starts_with('"') {
            SyntaxTok::Str
        } else if v == "true" || v == "false" {
            SyntaxTok::Bool
        } else if v == "null" || v == "nil" {
            SyntaxTok::Null
        } else if v.parse::<f64>().is_ok() {
            SyntaxTok::Num
        } else {
            SyntaxTok::Str
        };
        out.push((char_at(text, val.start()), char_at(text, val.end()), kind));
    }
    out
}

/// Non-overlapping match ranges `[start, end)` of `term` in `text`, in char units.
/// Literal by default; regex when set. An invalid regex matches nothing.
pub fn match_ranges(text: &str, term: &str, case_sensitive: bool, regex: bool) -> Vec<(usize, usize)> {
    if term.is_empty() {
        return Vec::new();
    }
    if regex {
        let re = match RegexBuilder::new(term)
            .case_insensitive(!case_sensitive)
            .build()
        {
            Ok(r) => r,
            Err(_) => return Vec::new(), // invalid pattern — match nothing
        };
        let mut out = Vec::new();
        for m in re.find_iter(text) {
            if m.start() == m.end() {
                continue; // skip zero-width
            }
            let s = text[..m.start()].chars().count();
            let e = text[..m.end()].chars().count();
            out.push((s, e));
        }
        return out;
    }

    let chars: Vec<char> = text.chars().collect();
    let needle: Vec<char> = term.chars().collect();
    let nlen = needle.len();
    if nlen == 0 || nlen > chars.len() {
        return Vec::new();
    }
    let eq = |a: char, b: char| {
        if case_sensitive {
            a == b
        } else {
            a.eq_ignore_ascii_case(&b)
        }
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i + nlen <= chars.len() {
        if (0..nlen).all(|k| eq(chars[i + k], needle[k])) {
            out.push((i, i + nlen));
            i += nlen; // non-overlapping
        } else {
            i += 1;
        }
    }
    out
}

pub fn build_spans(
    text: &str,
    highlights: &[HighlightSpec],
    search_term: &str,
    search_case: bool,
    search_regex: bool,
) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n == 0 {
        return Vec::new();
    }
    let mut color_at: Vec<Option<String>> = vec![None; n];
    let mut search_at = vec![false; n];

    for h in highlights {
        for (s, e) in match_ranges(text, &h.term, h.case_sensitive, h.regex) {
            for c in color_at.iter_mut().take(e).skip(s) {
                if c.is_none() {
                    *c = Some(h.color.clone());
                }
            }
        }
    }
    for (s, e) in match_ranges(text, search_term, search_case, search_regex) {
        for flag in search_at.iter_mut().take(e).skip(s) {
            *flag = true;
        }
    }

    let mut spans: Vec<Span> = Vec::new();
    let mut cur_key: Option<String> = None;
    for k in 0..n {
        let key = if search_at[k] {
            " search".to_string()
        } else {
            color_at[k].clone().unwrap_or_default()
        };
        if cur_key.as_deref() == Some(key.as_str()) {
            spans.last_mut().unwrap().text.push(chars[k]);
        } else {
            spans.push(if search_at[k] {
                Span { text: chars[k].to_string(), color: None, search: true }
            } else {
                Span { text: chars[k].to_string(), color: color_at[k].clone(), search: false }
            });
            cur_key = Some(key);
        }
    }
    spans
}

fn token_color(kind: JsonTokenType) -> Option<&'static str> {
    match kind {
        JsonTokenType::Key => Some("cyan"),
        JsonTokenType::String => Some("green"),
        JsonTokenType::Number => Some("yellow"),
        JsonTokenType::Boolean => Some("magenta"),
        JsonTokenType::Null => Some("gray"),
        JsonTokenType::Punct | JsonTokenType::Ws => None,
    }
}

/// Colour one pretty-printed JSON line: syntax colour as the base, with highlights
/// and the search term layered on. Precedence: search > highlight (bold) > syntax.
pub fn color_json_line(
    line: &str,
    highlights: &[HighlightSpec],
    search_term: &str,
    search_case: bool,
    search_regex: bool,
) -> Vec<JsonSpan> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    if n == 0 {
        return Vec::new();
    }
    let mut base: Vec<Option<String>> = vec![None; n];
    let mut i = 0;
    for tok in tokenize_json(line) {
        let c = token_color(tok.kind).map(str::to_string);
        for _ in tok.text.chars() {
            if i < n {
                base[i] = c.clone();
                i += 1;
            }
        }
    }
    let mut hl: Vec<Option<String>> = vec![None; n];
    for h in highlights {
        for (s, e) in match_ranges(line, &h.term, h.case_sensitive, h.regex) {
            for c in hl.iter_mut().take(e).skip(s) {
                if c.is_none() {
                    *c = Some(h.color.clone());
                }
            }
        }
    }
    let mut search_at = vec![false; n];
    for (s, e) in match_ranges(line, search_term, search_case, search_regex) {
        for flag in search_at.iter_mut().take(e).skip(s) {
            *flag = true;
        }
    }

    let mut spans: Vec<JsonSpan> = Vec::new();
    let mut cur_key: Option<String> = None;
    for k in 0..n {
        let is_hl = hl[k].is_some() && !search_at[k];
        let color = if search_at[k] {
            None
        } else {
            hl[k].clone().or_else(|| base[k].clone())
        };
        let key = format!(
            "{}{}{}",
            if search_at[k] { "S" } else { "" },
            color.clone().unwrap_or_default(),
            if is_hl { "H" } else { "" }
        );
        if cur_key.as_deref() == Some(key.as_str()) {
            spans.last_mut().unwrap().text.push(chars[k]);
        } else {
            spans.push(JsonSpan {
                text: chars[k].to_string(),
                color,
                search: search_at[k],
                hl: is_hl,
            });
            cur_key = Some(key);
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hs(term: &str, color: &str) -> HighlightSpec {
        HighlightSpec { term: term.into(), color: color.into(), case_sensitive: false, regex: false }
    }
    fn plain(text: &str) -> Span {
        Span { text: text.into(), color: None, search: false }
    }
    fn colored(text: &str, color: &str) -> Span {
        Span { text: text.into(), color: Some(color.into()), search: false }
    }

    #[test]
    fn single_plain_span_when_nothing_matches() {
        assert_eq!(build_spans("hello world", &[], "", false, false), vec![plain("hello world")]);
    }

    #[test]
    fn empty_text_yields_no_spans() {
        assert_eq!(build_spans("", &[hs("x", "#fff")], "y", false, false), vec![]);
    }

    #[test]
    fn highlight_case_insensitive_by_default() {
        assert_eq!(
            build_spans("an ERROR here", &[hs("error", "#f00")], "", false, false),
            vec![plain("an "), colored("ERROR", "#f00"), plain(" here")]
        );
    }

    #[test]
    fn highlight_respects_case_sensitive() {
        let h = HighlightSpec { term: "error".into(), color: "#f00".into(), case_sensitive: true, regex: false };
        assert_eq!(
            build_spans("Error error", &[h], "", false, false),
            vec![plain("Error "), colored("error", "#f00")]
        );
    }

    #[test]
    fn search_wins_over_highlight() {
        let spans = build_spans("foobar", &[hs("foo", "#0f0")], "foo", false, false);
        assert_eq!(spans[0], Span { text: "foo".into(), color: None, search: true });
        assert_eq!(spans[1], plain("bar"));
    }

    #[test]
    fn earlier_highlight_wins() {
        let spans = build_spans("abcd", &[hs("abc", "#111"), hs("bcd", "#222")], "", false, false);
        assert_eq!(spans, vec![colored("abc", "#111"), colored("d", "#222")]);
    }

    #[test]
    fn regex_highlight_and_invalid() {
        let h = HighlightSpec { term: "\\d+".into(), color: "#abc".into(), case_sensitive: false, regex: true };
        let spans = build_spans("id=42 id=7", &[h], "", false, false);
        let colored: Vec<String> = spans.iter().filter(|s| s.color.as_deref() == Some("#abc")).map(|s| s.text.clone()).collect();
        assert_eq!(colored, vec!["42", "7"]);

        let bad = HighlightSpec { term: "(".into(), color: "#abc".into(), case_sensitive: false, regex: true };
        assert_eq!(build_spans("abc", &[bad], "", false, false), vec![plain("abc")]);
    }

    #[test]
    fn multiple_occurrences() {
        let spans = build_spans("xax xax", &[hs("x", "#abc")], "", false, false);
        assert_eq!(spans.iter().filter(|s| s.color.as_deref() == Some("#abc")).count(), 4);
    }

    #[test]
    fn json_syntax_base_colours() {
        let spans = color_json_line("  \"user\": \"alice\"", &[], "", false, false);
        assert_eq!(spans.iter().find(|s| s.text == "\"user\"").unwrap().color.as_deref(), Some("cyan"));
        assert_eq!(spans.iter().find(|s| s.text == "\"alice\"").unwrap().color.as_deref(), Some("green"));
    }

    #[test]
    fn json_highlight_overrides_and_bolds() {
        let spans = color_json_line("  \"user\": \"alice\"", &[hs("alice", "#abc")], "", false, false);
        let hit = spans.iter().find(|s| s.text == "alice").unwrap();
        assert_eq!(hit.color.as_deref(), Some("#abc"));
        assert!(hit.hl);
    }

    #[test]
    fn syntax_logfmt_and_json() {
        // logfmt: key + number
        let r = syntax_ranges("ts=1 level=info abcd=3.0 msg=\"hi\"");
        assert!(r.iter().any(|(_, _, k)| *k == SyntaxTok::Key));
        assert!(r.iter().any(|(_, _, k)| *k == SyntaxTok::Num));
        assert!(r.iter().any(|(_, _, k)| *k == SyntaxTok::Str));

        // JSON: whole-line objects use the JSON tokenizer.
        let r = syntax_ranges(r#"{"blah":"special","n":42,"ok":true,"g":null}"#);
        assert!(r.iter().any(|(_, _, k)| *k == SyntaxTok::Key));
        assert!(r.iter().any(|(_, _, k)| *k == SyntaxTok::Num));
        assert!(r.iter().any(|(_, _, k)| *k == SyntaxTok::Bool));
        assert!(r.iter().any(|(_, _, k)| *k == SyntaxTok::Null));

        // Plain prose: no ranges (avoids noise).
        assert!(syntax_ranges("just a plain log message here").is_empty());

        // Comma-separated pairs (no space between): two independent pairs, not
        // one pair with value `1,b=2`.
        let src = "a=1,b=2";
        let r = syntax_ranges(src);
        let toks: Vec<(&str, SyntaxTok)> =
            r.iter().map(|(s, e, k)| (&src[*s..*e], *k)).collect();
        assert_eq!(
            toks,
            vec![
                ("a", SyntaxTok::Key),
                ("1", SyntaxTok::Num),
                ("b", SyntaxTok::Key),
                ("2", SyntaxTok::Num),
            ]
        );
    }

    #[test]
    fn json_search_wins() {
        let spans = color_json_line("  \"user\": \"alice\"", &[hs("alice", "#abc")], "alice", false, false);
        let hit = spans.iter().find(|s| s.text == "alice").unwrap();
        assert!(hit.search);
        assert!(hit.color.is_none());
    }
}
