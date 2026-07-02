//! Canonical, severity-ordered log levels. The index into [`LEVEL_NAMES`] *is*
//! the stored code (0 = none/unknown), so codes are stable and comparable.
//!
//! Ported from the Node app's `src/server/logParse.ts` (names + aliases) and
//! `src/client/lib/levels.ts` (colours), consolidated here since both the engine
//! (level detection) and the TUI (gutter tint) need them.

/// Level code 0..=6. Index into [`LEVEL_NAMES`].
pub type Level = u8;

pub const LEVEL_NONE: Level = 0;

pub const LEVEL_NAMES: [&str; 7] = ["", "TRACE", "DEBUG", "INFO", "WARN", "ERROR", "FATAL"];

/// Every alias word recognised by the level detectors, lower-cased. Used to build
/// the detection regexes in `engine::parse`.
pub const ALIAS_WORDS: [&str; 22] = [
    "trace",
    "finest",
    "finer",
    "debug",
    "fine",
    "dbg",
    "info",
    "information",
    "informational",
    "notice",
    "warn",
    "warning",
    "error",
    "err",
    "severe",
    "fatal",
    "crit",
    "critical",
    "emerg",
    "emergency",
    "alert",
    "panic",
];

/// Map a lower-cased synonym onto its canonical level code (`None` if unknown).
pub fn alias(word: &str) -> Option<Level> {
    Some(match word {
        "trace" | "finest" | "finer" => 1,
        "debug" | "fine" | "dbg" => 2,
        "info" | "information" | "informational" | "notice" => 3,
        "warn" | "warning" => 4,
        "error" | "err" | "severe" => 5,
        "fatal" | "crit" | "critical" | "emerg" | "emergency" | "alert" | "panic" => 6,
        _ => return None,
    })
}

/// Single-letter severities (MongoDB: F/E/W/I/D; some loggers add T). Input is
/// expected lower-cased.
pub fn letter_level(c: char) -> Option<Level> {
    Some(match c {
        'f' => 6,
        'e' => 5,
        'w' => 4,
        'i' => 3,
        'd' => 2,
        't' => 1,
        _ => return None,
    })
}

/// pino/bunyan numeric scale (10..=60).
pub fn pino_level(n: i64) -> Option<Level> {
    Some(match n {
        10 => 1,
        20 => 2,
        30 => 3,
        40 => 4,
        50 => 5,
        60 => 6,
        _ => return None,
    })
}

/// syslog severity 0..=7 → canonical: 0-2 emerg/alert/crit→fatal, 3 err→error,
/// 4 warn, 5 notice & 6 info→info, 7 debug.
pub const SYSLOG_LEVEL: [Level; 8] = [6, 6, 6, 5, 4, 3, 3, 2];

pub fn level_name(code: Level) -> &'static str {
    LEVEL_NAMES.get(code as usize).copied().unwrap_or("")
}

/// Hex colour for the gutter tint (`None` for code 0). Values match the web app.
pub fn level_color(code: Level) -> Option<&'static str> {
    Some(match code {
        1 => "#9e9e9e", // TRACE
        2 => "#78909c", // DEBUG
        3 => "#4caf50", // INFO
        4 => "#ffa726", // WARN
        5 => "#ef5350", // ERROR
        6 => "#c2185b", // FATAL
        _ => return None,
    })
}

/// Colour for the "no level" gutter (code 0).
pub const LEVEL_NONE_COLOR: &str = "#90a4ae";

/// RGB for the gutter tint, for terminal rendering (`None` for code 0).
pub fn level_rgb(code: Level) -> Option<(u8, u8, u8)> {
    level_color(code).map(hex_rgb)
}

/// Parse a `#rrggbb` string into an `(r, g, b)` triple.
pub fn hex_rgb(hex: &str) -> (u8, u8, u8) {
    let h = hex.strip_prefix('#').unwrap_or(hex);
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0);
    (byte(0), byte(2), byte(4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_codes_are_stable() {
        assert_eq!(level_name(0), "");
        assert_eq!(level_name(3), "INFO");
        assert_eq!(level_name(6), "FATAL");
        assert_eq!(level_name(99), "");
    }

    #[test]
    fn aliases_map_to_canonical_codes() {
        assert_eq!(alias("err"), Some(5));
        assert_eq!(alias("warning"), Some(4));
        assert_eq!(alias("notice"), Some(3));
        assert_eq!(alias("panic"), Some(6));
        assert_eq!(alias("nope"), None);
    }

    #[test]
    fn every_alias_word_resolves() {
        for w in ALIAS_WORDS {
            assert!(alias(w).is_some(), "alias {w} should resolve");
        }
    }

    #[test]
    fn numeric_scales() {
        assert_eq!(pino_level(30), Some(3));
        assert_eq!(pino_level(50), Some(5));
        assert_eq!(pino_level(3), None);
        assert_eq!(SYSLOG_LEVEL[3], 5);
        assert_eq!(SYSLOG_LEVEL[4], 4);
    }

    #[test]
    fn letters() {
        assert_eq!(letter_level('e'), Some(5));
        assert_eq!(letter_level('i'), Some(3));
        assert_eq!(letter_level('x'), None);
    }

    #[test]
    fn colours() {
        assert_eq!(level_color(5), Some("#ef5350"));
        assert_eq!(level_color(0), None);
        assert_eq!(level_rgb(5), Some((0xef, 0x53, 0x50)));
        assert_eq!(hex_rgb("#4caf50"), (0x4c, 0xaf, 0x50));
    }
}
