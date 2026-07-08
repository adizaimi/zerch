//! Terminal setup and the event loop. Puts the terminal into raw mode + alternate
//! screen, runs the [`app::App`] until it quits, and restores the terminal on the
//! way out (including on panic, via a drop guard).

pub mod app;

use crate::engine::index::LineIndex;
use crate::view::state::ViewState;
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

pub fn run(index: LineIndex, file_name: String, initial_view: Option<ViewState>) -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let _guard = TermGuard;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(index, file_name);
    if let Some(v) = initial_view {
        app.apply_view(v);
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
            } else if let Event::Key(key) = event::read()? {
                app.handle_key(key);
                if app.suspend {
                    app.suspend = false;
                    suspend_to_background(&mut terminal)?;
                }
            }
        }
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
