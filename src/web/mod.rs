//! Local, single-user browser UI for zerch: `zerch <file> --web`.
//!
//! A thin, loopback-only "open this log in a browser tab" mode that reuses zerch's
//! own engine + view model. It is deliberately **not** a rebuild of the `zercher`
//! upload/multi-user server — the whole view (filters, highlights, bookmarks, hidden
//! ranges, search, selected line) lives in the URL as a `--view` token, so each tab
//! is naturally isolated and shareable, and the server keeps no per-tab state.
//!
//! `run` mirrors `tui::run`'s shape so `main.rs` dispatches with a one-line branch.

mod assets;
mod server;

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::engine::index::LineIndex;
use crate::session;
use crate::view::state::{serialize_view, ViewState};

/// Shared, mostly-immutable server state. The index is read-only (no lock); the two
/// `Mutex`es are the only mutable state — `last_saved` gates redundant session writes.
pub struct State {
    pub index: Arc<LineIndex>,
    pub file_name: String,
    /// Canonical path for session persistence (None for piped input).
    // Wired into request handling in a later commit (session persistence).
    #[allow(dead_code)]
    pub session_key: Option<PathBuf>,
    /// Last view token written to the session file, so unchanged tokens skip the disk.
    #[allow(dead_code)]
    pub last_saved: Mutex<Option<String>>,
    /// The seed token served to `GET /` (from `--view` or the restored session).
    pub initial_view: String,
}

/// Run the web UI. Binds loopback HTTP, opens the browser, and blocks in the request
/// loop until the process is interrupted (Ctrl+C). `initial_view` (from `--view`) wins
/// over any saved session, exactly as in `tui::run`.
pub fn run(
    index: LineIndex,
    file_name: String,
    initial_view: Option<ViewState>,
    session_key: Option<PathBuf>,
    restore_session: bool,
    port: Option<u16>,
) -> io::Result<()> {
    // Seed token: explicit --view first, else the file's saved session (unless --fresh).
    let initial_token = initial_view
        .map(|v| serialize_view(&v))
        .or_else(|| {
            if restore_session {
                session_key.as_deref().and_then(session::load)
            } else {
                None
            }
        })
        .unwrap_or_default();

    let state = Arc::new(State {
        index: Arc::new(index),
        file_name,
        session_key,
        last_saved: Mutex::new(None),
        initial_view: initial_token,
    });

    let addr = format!("127.0.0.1:{}", port.unwrap_or(0));
    let server = tiny_http::Server::http(&addr)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    let bound_port = server.server_addr().to_ip().map(|s| s.port()).unwrap_or(0);
    let url = format!("http://127.0.0.1:{bound_port}");
    eprintln!("zerch: web UI at {url}");
    open_browser(&url);

    // Blocks until the process is killed (Ctrl+C); no shutdown endpoint.
    server::serve(Arc::new(server), state, 4);
    Ok(())
}

/// Best-effort browser launch. Platform-aware so it works on the Debian target
/// (`xdg-open`) and on the macOS dev host (`open`); the URL is already printed as a
/// fallback, so a missing opener is a silent no-op.
fn open_browser(url: &str) {
    use std::process::Command;
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    let _ = cmd.spawn();
}
