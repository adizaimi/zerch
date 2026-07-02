//! Terminal setup and the event loop. Puts the terminal into raw mode + alternate
//! screen, runs the [`app::App`] until it quits, and restores the terminal on the
//! way out (including on panic, via a drop guard).

pub mod app;

use crate::engine::index::LineIndex;
use app::App;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;

// Restores the terminal when dropped, so a panic can't leave it in raw mode.
struct TermGuard;
impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub fn run(index: LineIndex, file_name: String) -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let _guard = TermGuard;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(index, file_name);

    while !app.should_quit {
        terminal.draw(|f| app.render(f))?;
        if let Event::Key(key) = event::read()? {
            app.handle_key(key);
        }
    }
    Ok(())
}
