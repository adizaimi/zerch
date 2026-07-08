//! zercher — a smarter terminal log viewer (binary entry point).
//!
//! Parses args, spools stdin to a temp file when the source is a pipe, guards
//! against a non-TTY *output*, indexes the file, and hands off to the TUI.
//! `--view <string>` restores a saved view (byte-compatible with the web app's
//! shareable URL).

use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use zercher::engine::index::{IndexOptions, LineIndex};
use zercher::tui;
use zercher::view::state::parse_view;

const USAGE: &str = "\
zercher — a less-but-smarter terminal log viewer

USAGE:
    zercher <logfile> [--view <string>]
    zercher -                        # read log from stdin (explicit)
    <cmd> | zercher                  # read log from stdin (implicit)

OPTIONS:
    --view <string>    Restore a saved view (filters, highlights, bookmarks,
                       revealed ranges, search, selected line). Byte-compatible
                       with the zercher web app's shareable URL.
    -h, --help         Print this help and exit

Press ? inside the viewer for the interactive keybinding cheat-sheet.
";

// Kept alive for the process lifetime when we spooled stdin, so its Drop deletes
// the temp file on exit (normal or panic). Bound but unused after this point.
#[allow(dead_code)]
enum SourceGuard {
    File,
    Spooled(tempfile::TempPath),
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut file_arg: Option<String> = None;
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
        } else if file_arg.is_none() && (a == "-" || !a.starts_with('-')) {
            file_arg = Some(a.clone());
        }
        i += 1;
    }

    // Output must be a TTY (we draw a UI). Input from stdin is fine — that's
    // the whole point of piping; crossterm reads keyboard from /dev/tty on Unix.
    if !io::stdout().is_terminal() {
        eprintln!("zercher: stdout is not a TTY — run in an interactive terminal");
        return ExitCode::FAILURE;
    }

    // Decide the source: explicit `-`, no arg + piped stdin, or a real file.
    let mut from_pipe = false;
    let (path, display_name, _guard) = match file_arg.as_deref() {
        Some("-") => match spool_stdin() {
            Ok((p, g)) => {
                from_pipe = true;
                (p, "(stdin)".to_string(), g)
            }
            Err(e) => {
                eprintln!("zercher: could not spool stdin: {e}");
                return ExitCode::FAILURE;
            }
        },
        Some(f) => {
            let p = PathBuf::from(f);
            if !p.is_file() {
                eprintln!("zercher: no such file: {f}");
                return ExitCode::FAILURE;
            }
            (p, f.to_string(), SourceGuard::File)
        }
        None => {
            if io::stdin().is_terminal() {
                eprintln!("zercher: no log file given\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
            match spool_stdin() {
                Ok((p, g)) => {
                    from_pipe = true;
                    (p, "(stdin)".to_string(), g)
                }
                Err(e) => {
                    eprintln!("zercher: could not spool stdin: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    // When the log came from a pipe, fd 0 is that pipe — crossterm can't use it
    // for keyboard input or raw mode. Redirect fd 0 to /dev/tty so all further
    // stdin reads come from the terminal.
    if from_pipe {
        if let Err(e) = redirect_stdin_to_tty() {
            eprintln!("zercher: could not open /dev/tty for keyboard input: {e}");
            return ExitCode::FAILURE;
        }
    }

    let initial_view = view_str.as_deref().map(parse_view);

    // Size the rayon pool to the file before any parallel work: small files skip
    // the pool overhead, huge files scale up to every core. Must run before the
    // first parallel call (the index scan); build_global is a one-time setup.
    let file_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(zercher::plan_threads(file_len, cores))
        .build_global();

    // Transient status on the normal screen (no newline), erased once indexing is
    // done so it doesn't linger after the TUI (which uses the alternate screen) exits.
    eprint!("indexing {display_name}…");
    let _ = io::stderr().flush();
    let index = match LineIndex::open(&path, IndexOptions::default(), |_pct| {}) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("\r\x1b[2Kzercher: could not read {display_name}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprint!("\r\x1b[2K"); // carriage return + clear line
    let _ = io::stderr().flush();

    let code = match tui::run(index, display_name, initial_view) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zercher: {e}");
            ExitCode::FAILURE
        }
    };
    // _guard drops here → temp file (if any) unlinked.
    code
}

// Spool stdin to a temp file. Shows a byte-count progress line on stderr, erased
// on completion so it doesn't linger. Returns the temp file's path plus a Drop
// guard that unlinks it when the process ends.
fn spool_stdin() -> io::Result<(PathBuf, SourceGuard)> {
    let named = tempfile::Builder::new()
        .prefix("zercher-stdin-")
        .suffix(".log")
        .tempfile()?;
    let mut writer = io::BufWriter::new(named.as_file());
    let mut reader = io::stdin().lock();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    let mut next_report: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        total += n as u64;
        if total >= next_report {
            eprint!("\r\x1b[2Kspooling stdin… {}", fmt_bytes(total));
            let _ = io::stderr().flush();
            next_report = total + 1_048_576; // 1 MiB
        }
    }
    writer.flush()?;
    drop(writer);
    named.as_file().sync_all()?;
    eprint!("\r\x1b[2K"); // erase the "spooling…" line
    let _ = io::stderr().flush();
    let (_file, path) = named.keep().map_err(io::Error::from)?;
    // Re-wrap the kept path in a TempPath so Drop still unlinks it on exit.
    let tp = tempfile::TempPath::try_from_path(&path)?;
    Ok((path, SourceGuard::Spooled(tp)))
}

// Replace fd 0 with a fresh descriptor to /dev/tty so crossterm's stdin-based
// raw mode + key reads work when the log itself came in through a pipe.
#[cfg(unix)]
fn redirect_stdin_to_tty() -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let tty = std::fs::OpenOptions::new().read(true).open("/dev/tty")?;
    let ret = unsafe { libc::dup2(tty.as_raw_fd(), 0) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn redirect_stdin_to_tty() -> io::Result<()> {
    Err(io::Error::other(
        "piped input requires /dev/tty, which is not available on this platform",
    ))
}

fn fmt_bytes(n: u64) -> String {
    const K: u64 = 1024;
    if n < K {
        format!("{n} B")
    } else if n < K * K {
        format!("{:.1} KiB", n as f64 / K as f64)
    } else if n < K * K * K {
        format!("{:.1} MiB", n as f64 / (K * K) as f64)
    } else {
        format!("{:.2} GiB", n as f64 / (K * K * K) as f64)
    }
}
