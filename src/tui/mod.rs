//! Terminal setup and the event loop. Puts the terminal into raw mode + alternate
//! screen, runs the [`app::App`] until it quits, and restores the terminal on the
//! way out (including on panic, via a drop guard).

pub mod app;
mod history;

use crate::engine::index::LineIndex;
use crate::session;
use crate::view::state::{parse_view, ViewState};
use std::path::PathBuf;
use std::sync::Arc;
use app::App;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::time::Duration;

// Restores the terminal when dropped, so a panic can't leave it in raw mode.
struct TermGuard;
impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
}

/// Run the viewer. `initial_view` (from `--view`) wins over any saved session.
/// When `session_key` is set (a real file's canonical path), the view is saved
/// on exit and — unless `restore_session` is false (`--fresh`) — restored on
/// open when no `--view` was given.
pub fn run(
    index: Arc<LineIndex>,
    file_name: String,
    initial_view: Option<ViewState>,
    session_key: Option<PathBuf>,
    restore_session: bool,
) -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let _guard = TermGuard;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(index, file_name);
    // Seed the shared recent-input history from the persisted file (best-effort).
    app.set_history(history::load());
    // Restore: explicit --view first, else the file's saved session. Async: a
    // saved search/filter needs a whole-file scan, which would otherwise block
    // the first frame on a huge file — see `begin_apply_view_async`.
    let view = initial_view.or_else(|| {
        if restore_session {
            session_key.as_deref().and_then(session::load).map(|s| parse_view(&s))
        } else {
            None
        }
    });
    if let Some(v) = view {
        app.begin_apply_view_async(v);
    }

    while !app.should_quit {
        terminal.draw(|f| app.render(f))?;
        // Cooperative scheduling: if input is pending, handle it immediately
        // (user response wins). Otherwise, if we have per-term hit counts still
        // to scan for the menu, do ONE scan and loop back to redraw so the count
        // appears. Only when both queues are empty do we block for input.
        let handled = if event::poll(Duration::from_millis(0))? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(key);
                if app.suspend {
                    app.suspend = false;
                    suspend_to_background(&mut terminal)?;
                }
            }
            true
        } else {
            false
        };
        if !handled {
            if app.has_pending_counts() {
                app.compute_next_pending_count();
            } else if app.is_indexing() || app.has_pending_restore() {
                // Background metadata indexing and/or the async view-restore scan
                // are still running on their own thread(s) — there's nothing to
                // compute here, just wake up periodically to drain a ready restore
                // result and refresh the status bar's progress percentage, rather
                // than blocking indefinitely on the next keypress.
                app.poll_pending_restore();
                if event::poll(Duration::from_millis(100))? {
                    if let Event::Key(key) = event::read()? {
                        app.handle_key(key);
                        if app.suspend {
                            app.suspend = false;
                            suspend_to_background(&mut terminal)?;
                        }
                    }
                }
            } else if let Event::Key(key) = event::read()? {
                app.handle_key(key);
                if app.suspend {
                    app.suspend = false;
                    suspend_to_background(&mut terminal)?;
                }
            }
        }
    }
    // Persist the shared recent-input history for next time (best-effort).
    history::save(&app.history_snapshot());
    // Persist this file's view (filters/highlights/hidden/search + cursor line).
    if let Some(key) = &session_key {
        session::save(key, &app.view_string());
    }
    Ok(())
}

// Ctrl-Z: restore the terminal, stop ourselves with SIGTSTP (so the shell regains
// control), then re-enter raw mode + alt screen when brought back with `fg`.
#[cfg(unix)]
fn suspend_to_background(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, Show)?;
    // Deliver the default job-control stop; returns after SIGCONT (`fg`).
    unsafe {
        libc::raise(libc::SIGTSTP);
    }
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    terminal.clear()?; // force a full repaint of the restored screen
    Ok(())
}

#[cfg(not(unix))]
fn suspend_to_background(
    _terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> io::Result<()> {
    Ok(()) // no SIGTSTP equivalent; Ctrl-Z is a no-op
}
