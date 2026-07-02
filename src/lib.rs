//! zercher — a smarter terminal log viewer.
//!
//! The crate is split into an `engine` (indexing + search over a log file) and,
//! in later phases, a pure `view` model and a `tui` layer. Pure logic lives in
//! the library so it is unit-tested directly; the binary (`src/main.rs`) is a
//! thin shell over it.

pub mod engine;
pub mod levels;
pub mod tui;
pub mod view;
