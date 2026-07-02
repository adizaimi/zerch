//! Pure, testable view model shared by the TUI: the serialisable [`state::ViewState`]
//! (byte-compatible with the web app's shareable URL), the displayed-sequence merge,
//! text highlighting, and the grep/awk export.

pub mod displayed;
pub mod highlight;
pub mod jsonline;
pub mod state;

/// Highlight colours, assigned round-robin as highlights are added. Kept here (not
/// in `highlight`) because `state` also needs it to re-assign colours by index on
/// parse. Matches `src/tui/highlight.ts`.
pub const HIGHLIGHT_PALETTE: [&str; 10] = [
    "#4fc3f7", // light blue
    "#81c784", // green
    "#ba68c8", // purple
    "#ff8a65", // orange
    "#f06292", // pink
    "#4db6ac", // teal
    "#9575cd", // violet
    "#aed581", // lime
    "#ffb74d", // amber
    "#a1887f", // brown
];
