//! Detection, pretty-printing, and tokenising of JSON log lines — used by the
//! expanded JSON overlay and its syntax colouring. A line counts as JSON only when
//! it parses to an object/array (bare scalars don't get a useful multi-line view);
//! a "<meta> {json}" prefix is also accepted. Ported from `src/client/lib/jsonLine.ts`.

use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

pub struct JsonLine {
    pub value: Value,
    /// Text before the JSON span (e.g. a timestamp/level prefix), or "".
    pub prefix: String,
}

fn is_object_or_array(v: &Value) -> bool {
    matches!(v, Value::Object(_) | Value::Array(_))
}

/// Interpret `content` as a JSON log line, or `None` if it isn't one.
pub fn parse_json_line(content: &str) -> Option<JsonLine> {
    if content.is_empty() {
        return None;
    }
    let trimmed = content.trim();

    // Fast path: the whole (trimmed) line is a JSON object/array.
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if is_object_or_array(&v) {
                return Some(JsonLine {
                    value: v,
                    prefix: String::new(),
                });
            }
        }
    }

    // Prefixed payload: from the first opener to the last matching closer.
    let mut candidates: Vec<(usize, char)> = Vec::new();
    if let Some(i) = content.find('{') {
        candidates.push((i, '}'));
    }
    if let Some(i) = content.find('[') {
        candidates.push((i, ']'));
    }
    candidates.sort_by_key(|c| c.0);

    for (start, closer) in candidates {
        if let Some(end) = content.rfind(closer) {
            if end > start {
                if let Ok(v) = serde_json::from_str::<Value>(&content[start..=end]) {
                    if is_object_or_array(&v) {
                        return Some(JsonLine {
                            value: v,
                            prefix: content[..start].to_string(),
                        });
                    }
                }
            }
        }
    }

    None
}

/// Pretty-print a parsed value with 2-space indentation (matches JSON.stringify).
pub fn format_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JsonTokenType {
    Key,
    String,
    Number,
    Boolean,
    Null,
    Punct,
    Ws,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JsonToken {
    pub text: String,
    pub kind: JsonTokenType,
}

static JSON_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"("(?:\\.|[^"\\])*")(\s*:)|("(?:\\.|[^"\\])*")|(true|false)|(null)|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)|(\s+)|([{}\[\],:])"#,
    )
    .unwrap()
});

/// Single-pass tokeniser for pretty-printed JSON. A key is a string immediately
/// followed by `:`. Unrecognised text is emitted as `Punct` so nothing is lost.
pub fn tokenize_json(s: &str) -> Vec<JsonToken> {
    use JsonTokenType::*;
    let mut out = Vec::new();
    let mut last = 0;
    for caps in JSON_TOKEN_RE.captures_iter(s) {
        let m = caps.get(0).unwrap();
        if m.start() > last {
            out.push(JsonToken {
                text: s[last..m.start()].to_string(),
                kind: Punct,
            });
        }
        let push = |out: &mut Vec<JsonToken>, i: usize, kind: JsonTokenType| {
            if let Some(g) = caps.get(i) {
                out.push(JsonToken {
                    text: g.as_str().to_string(),
                    kind,
                });
            }
        };
        if let Some(k) = caps.get(1) {
            out.push(JsonToken { text: k.as_str().to_string(), kind: Key });
            out.push(JsonToken { text: caps[2].to_string(), kind: Punct }); // the ':'
        } else if caps.get(3).is_some() {
            push(&mut out, 3, String);
        } else if caps.get(4).is_some() {
            push(&mut out, 4, Boolean);
        } else if caps.get(5).is_some() {
            push(&mut out, 5, Null);
        } else if caps.get(6).is_some() {
            push(&mut out, 6, Number);
        } else if caps.get(7).is_some() {
            push(&mut out, 7, Ws);
        } else {
            push(&mut out, 8, Punct);
        }
        last = m.end();
    }
    if last < s.len() {
        out.push(JsonToken { text: s[last..].to_string(), kind: Punct });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_whole_line_object_and_array() {
        let r = parse_json_line(r#"{"a":1,"b":[2,3]}"#).unwrap();
        assert_eq!(r.prefix, "");
        assert_eq!(r.value, json!({"a":1,"b":[2,3]}));
        let r = parse_json_line("[1, 2, {\"x\": true}]").unwrap();
        assert_eq!(r.value, json!([1, 2, {"x": true}]));
    }

    #[test]
    fn extracts_prefixed_payload() {
        let r = parse_json_line(r#"2026-06-20 12:00:00 INFO {"msg":"hi","n":1}"#).unwrap();
        assert_eq!(r.prefix, "2026-06-20 12:00:00 INFO ");
        assert_eq!(r.value, json!({"msg":"hi","n":1}));
    }

    #[test]
    fn rejects_non_json_scalars_and_malformed() {
        for s in ["plain log message", "", "value: 42", "42", "\"just a string\"", "true", "{\"a\": }", "{not json}"] {
            assert!(parse_json_line(s).is_none(), "should reject: {s}");
        }
    }

    #[test]
    fn pretty_prints_two_space() {
        assert_eq!(format_json(&json!({"a":1})), "{\n  \"a\": 1\n}");
    }

    #[test]
    fn tokenizes_all_kinds() {
        let s = format_json(&json!({"name":"bob","age":42,"ok":true,"gone":null}));
        let toks: Vec<(JsonTokenType, String)> = tokenize_json(&s)
            .into_iter()
            .filter(|t| t.kind != JsonTokenType::Ws)
            .map(|t| (t.kind, t.text))
            .collect();
        use JsonTokenType as J;
        for expected in [
            (J::Key, "\"name\""),
            (J::String, "\"bob\""),
            (J::Number, "42"),
            (J::Boolean, "true"),
            (J::Null, "null"),
            (J::Punct, "{"),
            (J::Punct, ":"),
        ] {
            assert!(
                toks.iter().any(|(k, t)| *k == expected.0 && t == expected.1),
                "missing token {expected:?}"
            );
        }
    }

    #[test]
    fn tokenize_round_trips() {
        let s = format_json(&json!({"a":1,"b":"two","c":[true,null,-3.5e2],"k:colon":"v"}));
        let joined: String = tokenize_json(&s).into_iter().map(|t| t.text).collect();
        assert_eq!(joined, s);
    }
}
