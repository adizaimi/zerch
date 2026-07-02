//! Per-line structured parsing done during indexing: a timestamp and a log level.
//! Both are best-effort and return "none" when the line doesn't carry the field,
//! so plain-text lines simply have no timestamp/level. Pure (no I/O; no implicit
//! "now" except the optional `default_year` for year-less formats).
//!
//! Ported from `src/server/logParse.ts`; the test suite mirrors `logParse.test.ts`.

use crate::levels::{self, Level, LEVEL_NONE, SYSLOG_LEVEL};
use chrono::{Datelike, Local, TimeZone, Utc};
use regex::Regex;
use serde_json::{Map, Value};
use std::sync::LazyLock;

/// How to interpret a timestamp that carries NO explicit offset ("naive").
/// Timestamps that *do* carry an offset (Z, +00:00, -0700) are always absolute
/// and ignore this setting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NaiveZone {
    /// Treat the wall-clock as UTC (deterministic; the parser default).
    Utc,
    /// Treat it as the host's local time zone (DST-aware).
    Local,
    /// A fixed offset in minutes east of UTC (e.g. +120 for UTC+2).
    Offset(i32),
}

#[derive(Clone, Copy, Default)]
pub struct ParseOptions {
    /// Year for year-less formats (BSD syslog); defaults to the current UTC year.
    pub default_year: Option<i32>,
    /// Interpretation for offset-less timestamps; defaults to `Utc`.
    pub naive_zone: Option<NaiveZone>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Parsed {
    pub ts: Option<i64>,
    pub level: Level,
}

// Plausibility window for bare epoch numbers (2001-01-01 .. 2100-01-01).
const EPOCH_MIN_MS: i64 = 978_307_200_000;
const EPOCH_MAX_MS: i64 = 4_102_444_800_000;

// JSON field names, most specific first (`t`/`s`/`date` are generic → last).
const TS_KEYS: [&str; 7] = ["ts", "time", "timestamp", "@timestamp", "eventtime", "date", "t"];
const LEVEL_KEYS: [&str; 6] = ["level", "severity", "lvl", "loglevel", "levelname", "s"];

static ISO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[\[(]?(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2}):(\d{2})(?:[.,](\d{1,9}))?\s*(Z|[+-]\d{2}:?\d{2})?").unwrap()
});
static YMD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[\[(]?(\d{4})/(\d{2})/(\d{2})[T ](\d{2}):(\d{2}):(\d{2})(?:[.,](\d{1,9}))?")
        .unwrap()
});
static CLF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[?(\d{2})/([A-Za-z]{3})/(\d{4}):(\d{2}):(\d{2}):(\d{2})\s*([+-]\d{4})?").unwrap()
});
static BSD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Za-z]{3})\s+(\d{1,2})\s+(\d{2}):(\d{2}):(\d{2})").unwrap());
static EPOCH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[?(\d{10}|\d{13})(?:[\]\s,]|$)").unwrap());
static OFFSET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([+-])(\d{2}):?(\d{2})$").unwrap());
static ZONE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([+-])(\d{1,2}):?(\d{2})?$").unwrap());

static STRUCT_RE: LazyLock<Regex> = LazyLock::new(|| {
    let words = levels::ALIAS_WORDS.join("|");
    Regex::new(&format!(
        r#"(?i)\b(?:level|severity|lvl|loglevel)\s*[=:]\s*"?({words})"?"#
    ))
    .unwrap()
});
static BRACKET_RE: LazyLock<Regex> = LazyLock::new(|| {
    let words = levels::ALIAS_WORDS.join("|");
    Regex::new(&format!(r"(?i)[\[(]({words})[\])]")).unwrap()
});
static UPPER_RE: LazyLock<Regex> = LazyLock::new(|| {
    let upper = levels::ALIAS_WORDS
        .iter()
        .map(|w| w.to_uppercase())
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&format!(r"\b({upper})\b")).unwrap()
});

/// Parse a `ZERCHER_NAIVE_TZ`-style value into a [`NaiveZone`], or `None` if
/// unset/invalid. Accepts "utc"/"z", "local", or an offset like "+02:00"/"-0700".
pub fn resolve_naive_zone(value: Option<&str>) -> Option<NaiveZone> {
    let v = value?.trim().to_lowercase();
    if v.is_empty() {
        return None;
    }
    if v == "utc" || v == "z" {
        return Some(NaiveZone::Utc);
    }
    if v == "local" {
        return Some(NaiveZone::Local);
    }
    let c = ZONE_RE.captures(&v)?;
    let sign = if &c[1] == "-" { -1 } else { 1 };
    let h: i32 = c[2].parse().ok()?;
    let m: i32 = c.get(3).map(|x| x.as_str()).unwrap_or("0").parse().ok()?;
    Some(NaiveZone::Offset(sign * (h * 60 + m)))
}

// Keep only the first `max` characters (timestamps/levels live at the start),
// on a char boundary so multi-byte input never panics.
fn prefix(line: &str, max: usize) -> &str {
    match line.char_indices().nth(max) {
        Some((i, _)) => &line[..i],
        None => line,
    }
}

#[allow(clippy::too_many_arguments)] // mirrors the (y, mo, d, h, mi, s, ms) shape of the source
fn utc_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32, ms: i64) -> Option<i64> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, s)
        .single()
        .map(|dt| dt.timestamp_millis() + ms)
}

#[allow(clippy::too_many_arguments)] // mirrors the (y, mo, d, h, mi, s, ms, zone) shape of the source
fn naive_to_epoch(
    y: i32,
    mo: u32,
    d: u32,
    h: u32,
    mi: u32,
    s: u32,
    ms: i64,
    zone: NaiveZone,
) -> Option<i64> {
    match zone {
        NaiveZone::Local => Local
            .with_ymd_and_hms(y, mo, d, h, mi, s)
            .earliest()
            .map(|dt| dt.timestamp_millis() + ms),
        NaiveZone::Utc => utc_ms(y, mo, d, h, mi, s, ms),
        NaiveZone::Offset(off) => utc_ms(y, mo, d, h, mi, s, ms).map(|b| b - (off as i64) * 60_000),
    }
}

fn apply_offset(ms: i64, offset: Option<&str>) -> i64 {
    let off = match offset {
        Some(o) if !o.eq_ignore_ascii_case("z") => o,
        _ => return ms,
    };
    match OFFSET_RE.captures(off) {
        Some(c) => {
            let sign = if &c[1] == "-" { -1 } else { 1 };
            let mins = sign * (c[2].parse::<i64>().unwrap() * 60 + c[3].parse::<i64>().unwrap());
            ms - mins * 60_000
        }
        None => ms,
    }
}

fn frac(f: Option<&str>) -> i64 {
    match f {
        Some(s) => (format!("0.{s}").parse::<f64>().unwrap_or(0.0) * 1000.0).round() as i64,
        None => 0,
    }
}

fn month_num(name: &str) -> Option<u32> {
    Some(match name.to_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    })
}

/// Extract a timestamp from the start of a line, as epoch milliseconds, or `None`.
pub fn parse_timestamp(line: &str, opts: ParseOptions) -> Option<i64> {
    if line.is_empty() {
        return None;
    }
    let zone = opts.naive_zone.unwrap_or(NaiveZone::Utc);
    let s = prefix(line, 64);

    // ISO-8601
    if let Some(c) = ISO_RE.captures(s) {
        let (y, mo, d, h, mi, se) = ymdhms(&c);
        let ms = frac(c.get(7).map(|m| m.as_str()));
        return match c.get(8).map(|m| m.as_str()) {
            Some(off) => utc_ms(y, mo, d, h, mi, se, ms).map(|b| apply_offset(b, Some(off))),
            None => naive_to_epoch(y, mo, d, h, mi, se, ms, zone),
        };
    }

    // YYYY/MM/DD HH:MM:SS[.fff] — always naive (no offset in this format).
    if let Some(c) = YMD_RE.captures(s) {
        let (y, mo, d, h, mi, se) = ymdhms(&c);
        let ms = frac(c.get(7).map(|m| m.as_str()));
        return naive_to_epoch(y, mo, d, h, mi, se, ms, zone);
    }

    // Apache CLF: [10/Oct/2026:13:55:36 -0700]
    if let Some(c) = CLF_RE.captures(s) {
        if let Some(mo) = month_num(&c[2]) {
            let d: u32 = c[1].parse().unwrap();
            let y: i32 = c[3].parse().unwrap();
            let h: u32 = c[4].parse().unwrap();
            let mi: u32 = c[5].parse().unwrap();
            let se: u32 = c[6].parse().unwrap();
            return match c.get(7).map(|m| m.as_str()) {
                Some(raw) => {
                    let off = format!("{}:{}", &raw[0..3], &raw[3..]);
                    utc_ms(y, mo, d, h, mi, se, 0).map(|b| apply_offset(b, Some(&off)))
                }
                None => naive_to_epoch(y, mo, d, h, mi, se, 0, zone),
            };
        }
    }

    // BSD syslog: Jun 20 12:00:00 (no year, no offset).
    if let Some(c) = BSD_RE.captures(s) {
        if let Some(mo) = month_num(&c[1]) {
            let d: u32 = c[2].parse().unwrap();
            let h: u32 = c[3].parse().unwrap();
            let mi: u32 = c[4].parse().unwrap();
            let se: u32 = c[5].parse().unwrap();
            let year = opts.default_year.unwrap_or_else(|| Utc::now().year());
            return naive_to_epoch(year, mo, d, h, mi, se, 0, zone);
        }
    }

    // Bare epoch millis (13 digits) or seconds (10 digits).
    if let Some(c) = EPOCH_RE.captures(s) {
        let digits = &c[1];
        let n: i64 = digits.parse().unwrap();
        let ms = if digits.len() == 13 { n } else { n * 1000 };
        if (EPOCH_MIN_MS..=EPOCH_MAX_MS).contains(&ms) {
            return Some(ms);
        }
    }

    None
}

// Pull the mandatory Y-M-D H:M:S groups (1..=6) from an ISO/YMD capture.
fn ymdhms(c: &regex::Captures) -> (i32, u32, u32, u32, u32, u32) {
    (
        c[1].parse().unwrap(),
        c[2].parse().unwrap(),
        c[3].parse().unwrap(),
        c[4].parse().unwrap(),
        c[5].parse().unwrap(),
        c[6].parse().unwrap(),
    )
}

/// Detect the log level of a line (canonical code; 0 = none). Structured field →
/// bracketed `[LEVEL]` → standalone UPPERCASE token, in order of confidence.
pub fn parse_level(line: &str) -> Level {
    if line.is_empty() {
        return LEVEL_NONE;
    }
    let s = prefix(line, 80);
    for re in [&*STRUCT_RE, &*BRACKET_RE, &*UPPER_RE] {
        if let Some(c) = re.captures(s) {
            return levels::alias(&c[1].to_lowercase()).unwrap_or(LEVEL_NONE);
        }
    }
    LEVEL_NONE
}

fn epoch_from_number(n: f64) -> Option<i64> {
    if !n.is_finite() {
        return None;
    }
    let ms = (if n < 1e11 { n * 1000.0 } else { n }).round() as i64;
    (EPOCH_MIN_MS..=EPOCH_MAX_MS).contains(&ms).then_some(ms)
}

fn ts_from_value(v: &Value, zone: NaiveZone) -> Option<i64> {
    let opts = ParseOptions {
        naive_zone: Some(zone),
        ..Default::default()
    };
    match v {
        Value::Number(_) => epoch_from_number(v.as_f64()?),
        Value::String(s) => parse_timestamp(s, opts),
        Value::Object(m) => match m.get("$date")? {
            Value::String(s) => parse_timestamp(s, opts),
            n @ Value::Number(_) => epoch_from_number(n.as_f64()?),
            Value::Object(inner) => match inner.get("$numberLong") {
                Some(Value::String(long)) => epoch_from_number(long.parse::<f64>().ok()?),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

fn level_from_value(v: &Value) -> Level {
    match v {
        Value::Number(_) => {
            if let Some(n) = v.as_i64() {
                if let Some(p) = levels::pino_level(n) {
                    return p;
                }
                if (0..=7).contains(&n) {
                    return SYSLOG_LEVEL[n as usize];
                }
            }
            LEVEL_NONE
        }
        Value::String(s) => {
            let t = s.trim().to_lowercase();
            if let Some(a) = levels::alias(&t) {
                return a;
            }
            if t.chars().count() == 1 {
                return levels::letter_level(t.chars().next().unwrap()).unwrap_or(LEVEL_NONE);
            }
            LEVEL_NONE
        }
        _ => LEVEL_NONE,
    }
}

fn fields_from_object(m: &Map<String, Value>, zone: NaiveZone) -> Parsed {
    let mut ts = None;
    for k in TS_KEYS {
        if let Some(v) = m.get(k) {
            if let Some(r) = ts_from_value(v, zone) {
                ts = Some(r);
                break;
            }
        }
    }
    let mut level = LEVEL_NONE;
    for k in LEVEL_KEYS {
        if let Some(v) = m.get(k) {
            let r = level_from_value(v);
            if r != LEVEL_NONE {
                level = r;
                break;
            }
        }
    }
    Parsed { ts, level }
}

/// Parse a line's timestamp + level in one pass (used by the indexer). A JSON
/// object line is parsed once and read from its fields; everything else uses the
/// prefix scanners.
pub fn parse_line(line: &str, opts: ParseOptions) -> Parsed {
    if line.is_empty() {
        return Parsed {
            ts: None,
            level: LEVEL_NONE,
        };
    }
    let zone = opts.naive_zone.unwrap_or(NaiveZone::Utc);
    if line.trim_start_matches([' ', '\t']).starts_with('{') {
        if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(line) {
            return fields_from_object(&m, zone);
        }
    }
    Parsed {
        ts: parse_timestamp(line, opts),
        level: parse_level(line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ref_ms() -> i64 {
        utc_ms(2026, 6, 20, 12, 34, 56, 0).unwrap()
    }
    fn ts(line: &str) -> Option<i64> {
        parse_timestamp(line, ParseOptions::default())
    }
    // Level codes for readability.
    const TRACE: Level = 1;
    const DEBUG: Level = 2;
    const INFO: Level = 3;
    const WARN: Level = 4;
    const ERROR: Level = 5;
    const FATAL: Level = 6;

    #[test]
    fn iso_variants() {
        let r = ref_ms();
        assert_eq!(ts("2026-06-20T12:34:56Z hello"), Some(r));
        assert_eq!(ts("2026-06-20 12:34:56 INFO something"), Some(r));
        assert_eq!(ts("2026-06-20 12:34:56.123"), Some(r + 123));
        assert_eq!(ts("2026-06-20 12:34:56,5"), Some(r + 500));
        assert_eq!(ts("2026-06-20T12:34:56-05:00"), Some(r + 5 * 3_600_000));
        assert_eq!(ts("2026-06-20T12:34:56+02:00"), Some(r - 2 * 3_600_000));
    }

    #[test]
    fn other_formats() {
        let r = ref_ms();
        assert_eq!(ts("2026/06/20 12:34:56 msg"), Some(r));
        let clf = utc_ms(2026, 6, 20, 13, 55, 36, 0).unwrap() + 7 * 3_600_000;
        assert_eq!(ts(r#"[20/Jun/2026:13:55:36 -0700] "GET /""#), Some(clf));
        let bsd = parse_timestamp(
            "Jun 20 12:34:56 host sshd[1]: msg",
            ParseOptions {
                default_year: Some(2026),
                ..Default::default()
            },
        );
        assert_eq!(bsd, Some(r));
    }

    #[test]
    fn bare_epoch() {
        let r = ref_ms();
        assert_eq!(ts(&format!("{r} event")), Some(r));
        assert_eq!(ts(&format!("{} event", r / 1000)), Some(r));
        assert_eq!(ts(&format!("[{r}] event")), Some(r));
    }

    #[test]
    fn rejects_non_timestamps() {
        assert_eq!(ts("42 not a timestamp"), None);
        assert_eq!(ts("1234567 short"), None);
        assert_eq!(ts("plain log message"), None);
        assert_eq!(ts(""), None);
        assert_eq!(ts("ERROR something failed"), None);
        assert_eq!(ts("99999999999999999 huge"), None);
    }

    #[test]
    fn naive_zones() {
        let wall = "2026/06/22 16:47:39";
        let as_utc = utc_ms(2026, 6, 22, 16, 47, 39, 0).unwrap();
        assert_eq!(ts(wall), Some(as_utc));
        let z = |zone| parse_timestamp(wall, ParseOptions { naive_zone: Some(zone), ..Default::default() });
        assert_eq!(z(NaiveZone::Offset(120)), Some(as_utc - 120 * 60_000));
        assert_eq!(z(NaiveZone::Offset(-420)), Some(as_utc + 420 * 60_000));
        let local_expected = Local
            .with_ymd_and_hms(2026, 6, 22, 16, 47, 39)
            .earliest()
            .unwrap()
            .timestamp_millis();
        assert_eq!(z(NaiveZone::Local), Some(local_expected));
    }

    #[test]
    fn explicit_offset_ignores_naive_zone() {
        let as_utc = utc_ms(2026, 6, 22, 16, 47, 39, 0).unwrap();
        let opts = ParseOptions {
            naive_zone: Some(NaiveZone::Offset(120)),
            ..Default::default()
        };
        assert_eq!(parse_timestamp("2026-06-22T16:47:39Z", opts), Some(as_utc));
        assert_eq!(
            parse_timestamp("2026-06-22T16:47:39-07:00", opts),
            Some(as_utc + 7 * 3_600_000)
        );
    }

    #[test]
    fn resolve_zone() {
        assert_eq!(resolve_naive_zone(Some("utc")), Some(NaiveZone::Utc));
        assert_eq!(resolve_naive_zone(Some("UTC")), Some(NaiveZone::Utc));
        assert_eq!(resolve_naive_zone(Some("local")), Some(NaiveZone::Local));
        assert_eq!(resolve_naive_zone(Some("+02:00")), Some(NaiveZone::Offset(120)));
        assert_eq!(resolve_naive_zone(Some("-0700")), Some(NaiveZone::Offset(-420)));
        assert_eq!(resolve_naive_zone(Some("+5:30")), Some(NaiveZone::Offset(330)));
        assert_eq!(resolve_naive_zone(None), None);
        assert_eq!(resolve_naive_zone(Some("")), None);
        assert_eq!(resolve_naive_zone(Some("nonsense")), None);
    }

    #[test]
    fn levels_prefix_scanners() {
        assert_eq!(parse_level("2026-06-20T00:00:00Z ERROR boom"), ERROR);
        assert_eq!(parse_level("2026-06-20 00:00:00 INFO ok"), INFO);
        assert_eq!(parse_level("WARN something"), WARN);
        assert_eq!(parse_level("12:00:00 [error] failed"), ERROR);
        assert_eq!(parse_level("[Debug] verbose"), DEBUG);
        assert_eq!(parse_level("ts=1 level=warning msg=hi"), WARN);
        assert_eq!(parse_level("severity: CRITICAL boom"), FATAL);
        assert_eq!(parse_level("lvl=trace x=1"), TRACE);
        assert_eq!(parse_level("ERR connection"), ERROR);
        assert_eq!(parse_level("NOTICE fyi"), INFO);
    }

    #[test]
    fn levels_avoid_prose_and_none() {
        assert_eq!(parse_level("the error rate increased"), LEVEL_NONE);
        assert_eq!(parse_level("debugging the issue manually"), LEVEL_NONE);
        assert_eq!(parse_level("2026-06-20T00:00:00Z plain message"), LEVEL_NONE);
        assert_eq!(parse_level(""), LEVEL_NONE);
    }

    #[test]
    fn json_string_fields() {
        let r = utc_ms(2026, 6, 22, 14, 3, 30, 123).unwrap();
        let p = parse_line(
            r#"{"ts":"2026-06-22T14:03:30.123Z","level":"info","msg":"hi"}"#,
            ParseOptions::default(),
        );
        assert_eq!(p, Parsed { ts: Some(r), level: INFO });
        let p = parse_line(
            r#"{"@timestamp":"2026-06-22T14:03:30.123Z","severity":"ERROR"}"#,
            ParseOptions::default(),
        );
        assert_eq!(p, Parsed { ts: Some(r), level: ERROR });
    }

    #[test]
    fn json_numeric_levels() {
        let r = utc_ms(2026, 6, 22, 14, 3, 30, 123).unwrap();
        let p = parse_line(&format!(r#"{{"level":30,"time":{r},"msg":"x"}}"#), ParseOptions::default());
        assert_eq!(p, Parsed { ts: Some(r), level: INFO });
        assert_eq!(parse_line(r#"{"level":50,"time":0}"#, ParseOptions::default()).level, ERROR);
        assert_eq!(parse_line(r#"{"severity":3,"time":"2026-06-22T14:03:30Z"}"#, ParseOptions::default()).level, ERROR);
        assert_eq!(parse_line(r#"{"severity":4,"time":"2026-06-22T14:03:30Z"}"#, ParseOptions::default()).level, WARN);
    }

    #[test]
    fn json_mongodb_shapes() {
        let p = parse_line(
            r#"{"t":{"$date":"2026-06-22T14:31:05.123+00:00"},"s":"I","c":"NETWORK","msg":"x"}"#,
            ParseOptions::default(),
        );
        assert_eq!(p.ts, Some(utc_ms(2026, 6, 22, 14, 31, 5, 123).unwrap()));
        assert_eq!(p.level, INFO);
        assert_eq!(parse_line(r#"{"t":{"$date":"2026-06-22T14:31:05Z"},"s":"E"}"#, ParseOptions::default()).level, ERROR);
        assert_eq!(parse_line(r#"{"t":{"$date":"2026-06-22T14:31:05Z"},"s":"W"}"#, ParseOptions::default()).level, WARN);
        let r = utc_ms(2026, 6, 22, 14, 3, 30, 123).unwrap();
        let p = parse_line(&format!(r#"{{"t":{{"$date":{{"$numberLong":"{r}"}}}},"s":"D"}}"#), ParseOptions::default());
        assert_eq!(p, Parsed { ts: Some(r), level: DEBUG });
    }

    #[test]
    fn json_fallback_and_malformed() {
        let r = utc_ms(2026, 6, 22, 14, 3, 30, 123).unwrap();
        let p = parse_line("2026-06-22T14:03:30.123Z ERROR boom", ParseOptions::default());
        assert_eq!(p, Parsed { ts: Some(r), level: ERROR });
        let none = Parsed { ts: None, level: LEVEL_NONE };
        assert_eq!(parse_line("just a message", ParseOptions::default()), none);
        assert_eq!(parse_line("{not valid json", ParseOptions::default()), none);
        assert_eq!(parse_line("", ParseOptions::default()), none);
    }
}
