//! zercher — a smarter terminal log viewer (binary entry point).
//!
//! Parses args, guards against a non-TTY, indexes the file, and hands off to the
//! TUI. `--view <string>` restores a saved view (byte-compatible with the web
//! app's shareable URL).

use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

use zercher::engine::index::{IndexOptions, LineIndex};
use zercher::tui;
use zercher::view::state::parse_view;

const USAGE: &str = "\
zercher — a less-but-smarter terminal log viewer

USAGE:
    zercher <logfile> [--view <string>]

OPTIONS:
    --view <string>    Restore a saved view (filters, highlights, bookmarks,
                       revealed ranges, search, selected line). Byte-compatible
                       with the zercher web app's shareable URL.
    -h, --help         Print this help and exit

Press ? inside the viewer for the interactive keybinding cheat-sheet.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut file: Option<String> = None;
    let mut view_str: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-h" || a == "--help" {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        } else if a == "--view" {
            i += 1;
            view_str = args.get(i).cloned();
        } else if let Some(v) = a.strip_prefix("--view=") {
            view_str = Some(v.to_string());
        } else if file.is_none() && !a.starts_with('-') {
            file = Some(a.clone());
        }
        i += 1;
    }

    let Some(file) = file else {
        eprintln!("zercher: no log file given\n\n{USAGE}");
        return ExitCode::FAILURE;
    };

    if !io_is_tty() {
        eprintln!("zercher: stdin/stdout is not a TTY — run in an interactive terminal");
        return ExitCode::FAILURE;
    }

    let path = Path::new(&file);
    if !path.is_file() {
        eprintln!("zercher: no such file: {file}");
        return ExitCode::FAILURE;
    }

    let initial_view = view_str.as_deref().map(parse_view);

    eprintln!("indexing {file}…");
    let index = match LineIndex::open(path, IndexOptions::default(), |_pct| {}) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("zercher: could not read {file}: {e}");
            return ExitCode::FAILURE;
        }
    };

    match tui::run(index, file, initial_view) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zercher: {e}");
            ExitCode::FAILURE
        }
    }
}

fn io_is_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}
