//! Static frontend assets, baked into the binary with `include_str!` so the web UI
//! ships as a single self-contained executable (no runtime file lookups, nothing to
//! package separately). Served verbatim by `server.rs`.

pub const INDEX_HTML: &str = include_str!("assets/index.html");
pub const APP_CSS: &str = include_str!("assets/app.css");
pub const APP_JS: &str = include_str!("assets/app.js");
