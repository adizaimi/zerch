//! Translate the current filters + forced-visible line numbers into an equivalent
//! `grep`/`awk` command the user can run on their own copy of the log. Mirrors the
//! engine's matching semantics (literal→instr/grep -F, regex→awk `$0 ~ /pat/`,
//! case-insensitive default, negate→-v/`!(...)`, AND tighter than OR). A copy
//! helper, not a bit-exact compiler. Ported from `src/client/lib/shellCommand.ts`.

use crate::view::state::FilterMode;

#[derive(Clone, Debug)]
pub struct ShellFilter {
    pub term: String,
    pub case_sensitive: bool,
    pub regex: bool,
    pub negate: bool,
}

#[derive(Clone, Debug)]
pub struct ShellCommandOptions {
    /// Enabled filters only, in panel order.
    pub filters: Vec<ShellFilter>,
    pub filter_mode: FilterMode,
    /// Sorted, de-duped union of bookmarked + visible-range line numbers.
    pub forced_lines: Vec<usize>,
    /// The file the command should read.
    pub file_name: String,
}

// Wrap a string as one shell arg: single-quote, breaking out for embedded quotes.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// Escape for an awk double-quoted string literal (index() needle).
fn awk_string_literal(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

// Escape a regex for an awk /.../ literal (only the delimiter needs escaping).
fn awk_regex_literal(pattern: &str) -> String {
    format!("/{}/", pattern.replace('/', "\\/"))
}

fn push_run(parts: &mut Vec<String>, start: usize, end: usize) {
    parts.push(if start == end {
        format!("NR=={start}")
    } else {
        format!("(NR>={start} && NR<={end})")
    });
}

// Coalesce a sorted line list into awk NR tests, runs collapsed to ranges.
fn nr_conditions(lines: &[usize]) -> String {
    let mut sorted = lines.to_vec();
    sorted.sort_unstable();
    let mut parts: Vec<String> = Vec::new();
    let mut start: Option<usize> = None;
    let mut end = 0usize;
    for n in sorted {
        match start {
            None => {
                start = Some(n);
                end = n;
            }
            Some(_) if n == end + 1 => end = n,
            Some(st) => {
                push_run(&mut parts, st, end);
                start = Some(n);
                end = n;
            }
        }
    }
    if let Some(st) = start {
        push_run(&mut parts, st, end);
    }
    parts.join(" || ")
}

fn awk_term_expr(f: &ShellFilter) -> String {
    let expr = if f.regex {
        format!("$0 ~ {}", awk_regex_literal(&f.term))
    } else if f.case_sensitive {
        format!("index($0,{})", awk_string_literal(&f.term))
    } else {
        format!("index(tolower($0),{})", awk_string_literal(&f.term.to_lowercase()))
    };
    if f.negate {
        format!("!({expr})")
    } else {
        expr
    }
}

fn build_awk(opts: &ShellCommandOptions) -> String {
    let joiner = if opts.filter_mode == FilterMode::And { " && " } else { " || " };
    let filter_expr = opts
        .filters
        .iter()
        .map(awk_term_expr)
        .collect::<Vec<_>>()
        .join(joiner);

    let mut rules: Vec<String> = Vec::new();
    if !opts.forced_lines.is_empty() {
        rules.push(format!("{} {{ print; next }}", nr_conditions(&opts.forced_lines)));
    }
    rules.push(format!("{filter_expr} {{ print }}"));

    let ci_regex = opts.filters.iter().any(|f| f.regex && !f.case_sensitive);
    let begin = if ci_regex { "BEGIN { IGNORECASE = 1 } " } else { "" };
    let program = format!("{begin}{}", rules.join(" "));
    let cmd = format!("awk {} {}", shell_quote(&program), shell_quote(&opts.file_name));

    if opts.filters.iter().any(|f| f.regex) {
        format!(
            "# note: regex uses awk ERE (gawk); JS-specific syntax may differ. IGNORECASE is global.\n{cmd}"
        )
    } else {
        cmd
    }
}

fn build_grep_pipeline(opts: &ShellCommandOptions) -> String {
    let file = shell_quote(&opts.file_name);
    opts.filters
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let flags = format!(
                "{}{}F",
                if f.case_sensitive { "" } else { "i" },
                if f.negate { "v" } else { "" }
            );
            let grep = format!("grep -{flags} {}", shell_quote(&f.term));
            if i == 0 {
                format!("{grep} {file}")
            } else {
                grep
            }
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Build the shell command equivalent to the current viewer state.
pub fn build_shell_command(opts: &ShellCommandOptions) -> String {
    let file_name = if opts.file_name.is_empty() {
        "logfile".to_string()
    } else {
        opts.file_name.clone()
    };
    let filters: Vec<ShellFilter> = opts.filters.iter().filter(|f| !f.term.is_empty()).cloned().collect();
    let norm = ShellCommandOptions {
        filters,
        filter_mode: opts.filter_mode,
        forced_lines: opts.forced_lines.clone(),
        file_name: file_name.clone(),
    };

    // No filters: the viewer shows the whole file.
    if norm.filters.is_empty() {
        return format!("cat {}", shell_quote(&file_name));
    }

    // grep pipeline only for the always-faithful case: AND of literals, no forced lines.
    let grep_eligible = norm.filter_mode == FilterMode::And
        && norm.forced_lines.is_empty()
        && norm.filters.iter().all(|f| !f.regex);

    if grep_eligible {
        build_grep_pipeline(&norm)
    } else {
        build_awk(&norm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(term: &str) -> ShellFilter {
        ShellFilter { term: term.into(), case_sensitive: false, regex: false, negate: false }
    }
    fn opts(filters: Vec<ShellFilter>, mode: FilterMode, forced: Vec<usize>, file: &str) -> ShellCommandOptions {
        ShellCommandOptions { filters, filter_mode: mode, forced_lines: forced, file_name: file.into() }
    }
    use FilterMode::{And, Or};

    #[test]
    fn cat_when_no_filters() {
        assert_eq!(build_shell_command(&opts(vec![], And, vec![], "app.log")), "cat 'app.log'");
        assert_eq!(build_shell_command(&opts(vec![], And, vec![3, 4], "app.log")), "cat 'app.log'");
        assert_eq!(build_shell_command(&opts(vec![], And, vec![], "")), "cat 'logfile'");
        assert_eq!(build_shell_command(&opts(vec![], And, vec![], "my logs.txt")), "cat 'my logs.txt'");
    }

    #[test]
    fn grep_literals() {
        assert_eq!(
            build_shell_command(&opts(vec![f("error")], And, vec![], "app.log")),
            "grep -iF 'error' 'app.log'"
        );
        let cs = ShellFilter { case_sensitive: true, ..f("Error") };
        assert_eq!(
            build_shell_command(&opts(vec![cs], And, vec![], "app.log")),
            "grep -F 'Error' 'app.log'"
        );
        let neg = ShellFilter { negate: true, ..f("debug") };
        assert_eq!(
            build_shell_command(&opts(vec![f("timeout"), neg], And, vec![], "app.log")),
            "grep -iF 'timeout' 'app.log' | grep -ivF 'debug'"
        );
        assert_eq!(
            build_shell_command(&opts(vec![f("can't")], And, vec![], "app.log")),
            "grep -iF 'can'\\''t' 'app.log'"
        );
    }

    #[test]
    fn awk_for_or_and_forced_lines() {
        assert_eq!(
            build_shell_command(&opts(vec![f("a"), f("b")], Or, vec![], "app.log")),
            "awk 'index(tolower($0),\"a\") || index(tolower($0),\"b\") { print }' 'app.log'"
        );
        assert_eq!(
            build_shell_command(&opts(vec![f("warn")], And, vec![4, 7, 3, 5], "app.log")),
            "awk '(NR>=3 && NR<=5) || NR==7 { print; next } index(tolower($0),\"warn\") { print }' 'app.log'"
        );
        let cs = ShellFilter { case_sensitive: true, ..f("say \"hi\"") };
        assert_eq!(
            build_shell_command(&opts(vec![cs], Or, vec![], "app.log")),
            "awk 'index($0,\"say \\\"hi\\\"\") { print }' 'app.log'"
        );
    }

    #[test]
    fn awk_for_regex_with_caveat() {
        let rx = ShellFilter { regex: true, ..f("err\\d+") };
        assert_eq!(
            build_shell_command(&opts(vec![rx], And, vec![], "app.log")),
            "# note: regex uses awk ERE (gawk); JS-specific syntax may differ. IGNORECASE is global.\nawk 'BEGIN { IGNORECASE = 1 } $0 ~ /err\\d+/ { print }' 'app.log'"
        );
        let rx_cs = ShellFilter { regex: true, case_sensitive: true, ..f("ERR") };
        assert_eq!(
            build_shell_command(&opts(vec![rx_cs], And, vec![], "app.log")),
            "# note: regex uses awk ERE (gawk); JS-specific syntax may differ. IGNORECASE is global.\nawk '$0 ~ /ERR/ { print }' 'app.log'"
        );
        let rx_slash = ShellFilter { regex: true, case_sensitive: true, ..f("a/b") };
        assert!(build_shell_command(&opts(vec![rx_slash], And, vec![], "app.log")).contains("$0 ~ /a\\/b/"));
    }
}
