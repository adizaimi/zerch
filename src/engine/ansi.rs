//! Strip ANSI escape sequences (colour / cursor codes) from a line.
//!
//! CI logs are frequently colourised, and the escape bytes otherwise land
//! *inside* the indexed text — so a search for `FAIL:` misses `\x1b[31mFAIL\x1b[0m:`.
//! Stripping at index time keeps the stored text plain while the terminal still
//! renders our own styling. Ported verbatim from the Node indexer's `ANSI_RE`:
//! `\x1B(?:[@-Z\-_]|\[[0-?]*[ -/]*[@-~])` — a C1 escape (ESC + 0x40–0x5A/0x5C–0x5F,
//! i.e. everything but `[`) or a CSI sequence (`ESC [ … final`).

use regex::Regex;
use std::borrow::Cow;
use std::sync::LazyLock;

static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])").unwrap());

/// Remove ANSI escape sequences. Returns a borrowed slice when the input is
/// already clean (no allocation), otherwise an owned stripped `String`.
pub fn strip_ansi(s: &str) -> Cow<'_, str> {
    ANSI_RE.replace_all(s, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_sgr_colour_codes() {
        // The exact CI case that broke literal search for "FAIL:" in the Node app.
        let input = "=== \x1b[31mFAIL\x1b[0m: cmd/correctness (119.93s)";
        assert_eq!(strip_ansi(input), "=== FAIL: cmd/correctness (119.93s)");
    }

    #[test]
    fn strips_cursor_and_multiple_codes() {
        let input = "\x1b[1m\x1b[32mok\x1b[0m done\x1b[2K";
        assert_eq!(strip_ansi(input), "ok done");
    }

    #[test]
    fn leaves_plain_text_untouched_and_borrowed() {
        let out = strip_ansi("plain log message");
        assert_eq!(out, "plain log message");
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn empty_string() {
        assert_eq!(strip_ansi(""), "");
    }
}
