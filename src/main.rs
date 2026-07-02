//! zercher — a smarter terminal log viewer (binary entry point).
//!
//! Parses args, guards against a non-TTY, indexes the file, and hands off to the
//! TUI. `--view` restore lands in a later phase; for now the first non-flag arg is
//! the log file.

use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

use zercher::engine::index::{IndexOptions, LineIndex};
use zercher::tui;

const USAGE: &str = "\
zercher — a less-but-smarter terminal log viewer

USAGE:
    zercher <logfile>

OPTIONS:
    -h, --help    Print this help and exit

Press ? inside the viewer for the interactive keybinding cheat-sheet.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    let Some(file) = args.iter().find(|a| !a.starts_with('-')).cloned() else {
        eprintln!("zercher: no log file given");
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

    eprintln!("indexing {file}…");
    let index = match LineIndex::open(path, IndexOptions::default(), |_pct| {}) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("zercher: could not read {file}: {e}");
            return ExitCode::FAILURE;
        }
    };

    match tui::run(index, file) {
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
