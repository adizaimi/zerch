//! Serialise / parse the view (filters, highlights, bookmarks, revealed ranges,
//! search, selected line) to a compact **query-string token**: comma-separated
//! flag lists, `application/x-www-form-urlencoded` encoding, `bookmarks` as a plain
//! list, `visible` as runs. The token round-trips exactly through `--view`, so an
//! exact view can be saved or handed off as a single string. `henabled` (per-highlight
//! enable) and `scase` (search case) default sensibly when absent.

use crate::view::HIGHLIGHT_PALETTE;
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FilterMode {
    #[default]
    And,
    Or,
}

impl FilterMode {
    fn as_str(self) -> &'static str {
        match self {
            FilterMode::And => "AND",
            FilterMode::Or => "OR",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FilterEntry {
    pub term: String,
    pub enabled: bool,
    pub case_sensitive: bool,
    pub regex: bool,
    pub negate: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HighlightEntry {
    pub term: String,
    pub color: String,
    pub enabled: bool,
    pub case_sensitive: bool,
    pub regex: bool,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ViewState {
    pub filters: Vec<FilterEntry>,
    pub filter_mode: FilterMode,
    pub highlights: Vec<HighlightEntry>,
    pub bookmarks: Vec<usize>,
    pub visible: Vec<usize>, // "unhidden" (gap-revealed) line numbers
    pub hidden: Vec<usize>,  // explicitly hidden line numbers (the `x` command)
    pub search: String,
    pub search_regex: bool,
    pub search_case: bool,
    pub selected: Option<usize>, // cursor/anchored line (the `selected` param)
}

static RANGE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+)(?:-(\d+))?$").unwrap());
static DIGITS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+$").unwrap());

fn bits(flags: impl Iterator<Item = bool>) -> String {
    flags.map(|b| if b { "1" } else { "0" }).collect::<Vec<_>>().join(",")
}

/// Coalesce line numbers into "a-b,c" runs (the `visible` param encoding).
pub fn encode_runs(lines: &[usize]) -> String {
    let mut s: Vec<usize> = lines.to_vec();
    s.sort_unstable();
    s.dedup();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let mut j = i;
        while j + 1 < s.len() && s[j + 1] == s[j] + 1 {
            j += 1;
        }
        out.push(if i == j {
            s[i].to_string()
        } else {
            format!("{}-{}", s[i], s[j])
        });
        i = j + 1;
    }
    out.join(",")
}

/// Expand "a-b,c" (or a plain "a,b,c" list) into line numbers; ignores malformed.
pub fn parse_range_spec(spec: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for piece in spec.split(',') {
        if piece.is_empty() {
            continue;
        }
        if let Some(c) = RANGE_RE.captures(piece) {
            let lo: usize = c[1].parse().unwrap();
            let hi: usize = c.get(2).map(|m| m.as_str().parse().unwrap()).unwrap_or(lo);
            for n in lo.min(hi)..=lo.max(hi) {
                out.push(n);
            }
        }
    }
    out
}

pub fn serialize_view(v: &ViewState) -> String {
    let mut p = form_urlencoded::Serializer::new(String::new());

    let filters: Vec<&FilterEntry> = v.filters.iter().filter(|f| !f.term.is_empty()).collect();
    if !filters.is_empty() {
        p.append_pair("filters", &join_terms(filters.iter().map(|f| f.term.as_str())));
        p.append_pair("enabled", &bits(filters.iter().map(|f| f.enabled)));
        p.append_pair("mode", v.filter_mode.as_str());
        if filters.iter().any(|f| f.case_sensitive) {
            p.append_pair("fcase", &bits(filters.iter().map(|f| f.case_sensitive)));
        }
        if filters.iter().any(|f| f.regex) {
            p.append_pair("frx", &bits(filters.iter().map(|f| f.regex)));
        }
        if filters.iter().any(|f| f.negate) {
            p.append_pair("fneg", &bits(filters.iter().map(|f| f.negate)));
        }
    }

    if !v.bookmarks.is_empty() {
        let mut bm = v.bookmarks.clone();
        bm.sort_unstable();
        bm.dedup();
        p.append_pair("bookmarks", &join_nums(&bm));
    }
    if !v.visible.is_empty() {
        p.append_pair("visible", &encode_runs(&v.visible));
    }
    if !v.hidden.is_empty() {
        p.append_pair("hidden", &encode_runs(&v.hidden));
    }

    let hs: Vec<&HighlightEntry> = v.highlights.iter().filter(|h| !h.term.is_empty()).collect();
    if !hs.is_empty() {
        p.append_pair("highlights", &join_terms(hs.iter().map(|h| h.term.as_str())));
        if hs.iter().any(|h| h.case_sensitive) {
            p.append_pair("hcase", &bits(hs.iter().map(|h| h.case_sensitive)));
        }
        if hs.iter().any(|h| h.regex) {
            p.append_pair("hrx", &bits(hs.iter().map(|h| h.regex)));
        }
        if hs.iter().any(|h| !h.enabled) {
            p.append_pair("henabled", &bits(hs.iter().map(|h| h.enabled)));
        }
    }

    if !v.search.is_empty() {
        p.append_pair("search", &v.search);
        if v.search_regex {
            p.append_pair("srx", "1");
        }
        if v.search_case {
            p.append_pair("scase", "1");
        }
    }

    if let Some(sel) = v.selected {
        p.append_pair("selected", &sel.to_string());
    }

    p.finish()
}

pub fn parse_view(query: &str) -> ViewState {
    let q = query.strip_prefix('?').unwrap_or(query);
    let mut map: HashMap<String, String> = HashMap::new();
    for (k, val) in form_urlencoded::parse(q.as_bytes()) {
        map.entry(k.into_owned()).or_insert_with(|| val.into_owned());
    }

    let list = |key: &str| -> Vec<String> {
        match map.get(key) {
            Some(v) if !v.is_empty() => v.split(',').map(str::to_string).collect(),
            _ => Vec::new(),
        }
    };
    let flags = |key: &str| -> Vec<bool> { list(key).iter().map(|b| b == "1").collect() };
    let flag_at = |arr: &[bool], i: usize, dflt: bool| arr.get(i).copied().unwrap_or(dflt);

    let f_terms = list("filters");
    let (f_en, f_case, f_rx, f_neg) = (flags("enabled"), flags("fcase"), flags("frx"), flags("fneg"));
    let filters = f_terms
        .into_iter()
        .enumerate()
        .map(|(i, term)| FilterEntry {
            term,
            enabled: flag_at(&f_en, i, true),
            case_sensitive: flag_at(&f_case, i, false),
            regex: flag_at(&f_rx, i, false),
            negate: flag_at(&f_neg, i, false),
        })
        .collect();

    let h_terms = list("highlights");
    let (h_case, h_rx, h_en) = (flags("hcase"), flags("hrx"), flags("henabled"));
    let highlights = h_terms
        .into_iter()
        .enumerate()
        .map(|(i, term)| HighlightEntry {
            color: HIGHLIGHT_PALETTE[i % HIGHLIGHT_PALETTE.len()].to_string(),
            term,
            enabled: flag_at(&h_en, i, true),
            case_sensitive: flag_at(&h_case, i, false),
            regex: flag_at(&h_rx, i, false),
        })
        .collect();

    let get = |k: &str| map.get(k).cloned().unwrap_or_default();
    let selected = map
        .get("selected")
        .filter(|s| DIGITS_RE.is_match(s))
        .and_then(|s| s.parse().ok());

    ViewState {
        filters,
        filter_mode: if get("mode") == "OR" { FilterMode::Or } else { FilterMode::And },
        highlights,
        bookmarks: parse_range_spec(&get("bookmarks")),
        visible: parse_range_spec(&get("visible")),
        hidden: parse_range_spec(&get("hidden")),
        search: get("search"),
        search_regex: get("srx") == "1",
        search_case: get("scase") == "1",
        selected,
    }
}

fn join_terms<'a>(terms: impl Iterator<Item = &'a str>) -> String {
    terms.collect::<Vec<_>>().join(",")
}
fn join_nums(nums: &[usize]) -> String {
    nums.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ViewState {
        ViewState::default()
    }

    #[test]
    fn runs_round_trip() {
        assert_eq!(encode_runs(&[1, 2, 3, 5, 8, 9]), "1-3,5,8-9");
        assert_eq!(parse_range_spec("1-3,5,8-9"), vec![1, 2, 3, 5, 8, 9]);
        assert_eq!(parse_range_spec("4,,x,7-"), vec![4]);
    }

    #[test]
    fn filters_round_trip_with_flags_and_mode() {
        let v = ViewState {
            filter_mode: FilterMode::Or,
            filters: vec![
                FilterEntry { term: "ERROR".into(), enabled: true, case_sensitive: false, regex: false, negate: false },
                FilterEntry { term: "time out".into(), enabled: false, case_sensitive: true, regex: true, negate: true },
            ],
            ..base()
        };
        let back = parse_view(&serialize_view(&v));
        assert_eq!(back.filter_mode, FilterMode::Or);
        assert_eq!(back.filters, v.filters); // space in a term survives
    }

    #[test]
    fn comma_separated_flag_lists_are_web_compatible() {
        let s = serialize_view(&ViewState {
            filters: vec![
                FilterEntry { term: "a".into(), enabled: true, case_sensitive: true, regex: false, negate: false },
                FilterEntry { term: "b".into(), enabled: false, case_sensitive: false, regex: false, negate: false },
            ],
            ..base()
        });
        assert!(s.contains("enabled=1%2C0"), "got: {s}");
        assert!(s.contains("fcase=1%2C0"), "got: {s}");
    }

    #[test]
    fn parses_full_query_string_token() {
        let token = "filters=error&enabled=1&fcase=0&frx=0&fneg=0&mode=AND&highlights=error,producing,record,at&hcase=0,0,0,0&hrx=0,0,0,0&bookmarks=4,7&search=%7B";
        let v = parse_view(token);
        assert_eq!(
            v.filters,
            vec![FilterEntry { term: "error".into(), enabled: true, case_sensitive: false, regex: false, negate: false }]
        );
        assert_eq!(
            v.highlights.iter().map(|h| h.term.clone()).collect::<Vec<_>>(),
            vec!["error", "producing", "record", "at"]
        );
        assert!(v.highlights.iter().all(|h| h.enabled)); // no henabled → default on
        assert_eq!(v.bookmarks, vec![4, 7]);
        assert_eq!(v.search, "{");
    }

    #[test]
    fn highlights_and_search_flags_round_trip() {
        let v = ViewState {
            highlights: vec![HighlightEntry {
                term: "INFO".into(),
                color: "#000".into(),
                enabled: true,
                case_sensitive: true,
                regex: false,
            }],
            search: "time out".into(),
            search_regex: true,
            search_case: true,
            ..base()
        };
        let back = parse_view(&serialize_view(&v));
        assert_eq!(back.highlights[0].term, "INFO");
        assert!(back.highlights[0].case_sensitive);
        assert!(back.highlights[0].enabled);
        assert_eq!(back.search, "time out");
        assert!(back.search_regex);
        assert!(back.search_case);
    }

    #[test]
    fn bookmarks_and_visible_round_trip() {
        let v = ViewState {
            bookmarks: vec![4, 7],
            visible: vec![3, 4, 5, 8],
            hidden: vec![10, 11, 12, 20],
            ..base()
        };
        let back = parse_view(&serialize_view(&v));
        assert_eq!(back.bookmarks, vec![4, 7]);
        assert_eq!(back.visible, vec![3, 4, 5, 8]);
        assert_eq!(back.hidden, vec![10, 11, 12, 20]);
    }

    #[test]
    fn selected_round_trip() {
        assert_eq!(parse_view(&serialize_view(&ViewState { selected: Some(120), ..base() })).selected, Some(120));
        assert_eq!(parse_view("selected=42").selected, Some(42));
        assert_eq!(parse_view("").selected, None);
    }

    #[test]
    fn empty_view_is_empty_string() {
        assert_eq!(serialize_view(&base()), "");
        let back = parse_view("");
        assert!(back.filters.is_empty());
        assert!(back.highlights.is_empty());
    }
}
