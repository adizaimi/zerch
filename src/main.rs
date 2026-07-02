//! zercher — a smarter terminal log viewer.
//!
//! Phase 0 scaffold: this entry point only parses `--help` and prints usage.
//! Indexing, search, and the TUI land in subsequent phases.

use std::process::ExitCode;

const USAGE: &str = "\
zercher — a less-but-smarter terminal log viewer

USAGE:
    zercher <logfile> [--view <string>]

ARGS:
    <logfile>          Path to the log file to open

OPTIONS:
    --view <string>    Restore a saved view (filters, highlights, bookmarks,
                       revealed ranges, search, selected line). Byte-compatible
                       with the zercher web app's shareable URL.
    -h, --help         Print this help and exit

Press ? inside the viewer for the interactive keybinding cheat-sheet.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    // Real behavior arrives in later phases; fail loudly until then.
    eprintln!("zercher: not yet implemented (Phase 0 scaffold)");
    ExitCode::FAILURE
}
