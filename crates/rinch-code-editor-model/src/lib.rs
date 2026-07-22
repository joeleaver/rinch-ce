//! The editor model — the one thing that owns the whole document.
//!
//! The buffer crate holds the parts that are easy to get subtly wrong in
//! isolation (rope storage, multi-cursor edit composition, exact undo); the
//! syntax crate turns text into tokens. Neither knows about the other, and
//! nothing wires them together. That wiring is this crate.
//!
//! [`EditorModel`] owns a [`Buffer`], its [`Selections`], an undo [`History`],
//! and a [`Highlighter`], and exposes a small set of *semantic* edit operations
//! — `insert`, `backspace`, `delete_forward`, `cut`, `paste`, `undo`, `redo`.
//! Each one does the four things that must always happen together, in the one
//! order that keeps them consistent:
//!
//! 1. derive the low-level [`Edit`]s from the current selections,
//! 2. apply them to the buffer as one atomic [`Change`],
//! 3. move every selection to where it belongs *after* the edit,
//! 4. record the change in history and forward it to the highlighter.
//!
//! Doing any of those out of order — rebasing a selection against a buffer that
//! has not been edited yet, or telling the highlighter about a change the buffer
//! has not seen — corrupts state in ways that are miserable to debug. Keeping
//! the sequence in exactly one place is the whole point of the crate.
//!
//! # Still no rinch, still no clock
//!
//! Like the crates beneath it, this one depends on neither rinch nor a renderer,
//! so the orchestration can be exhaustively unit-tested without a GUI. And it
//! still has no clock: undo coalescing is decided from the *shape* of the edits
//! (a run of single-character typing groups together; a newline, a paste, a
//! deletion, or any caret move starts a fresh group) rather than from elapsed
//! time — the same reason spelled out in the buffer crate's docs.
//!
//! # What the view still owns
//!
//! Cursor *motion* (left/right/up/down, word-wise, home/end) and all pixel
//! geometry live above this crate. The model moves selections only as a
//! consequence of an edit; asking it to move the caret one column left is the
//! view layer's job, because that needs the visual line model the view builds.

mod clipboard;
mod cursor;
mod geometry;
mod graphemes;
mod model;
mod select;

pub use clipboard::ClipboardPayload;
pub use cursor::Motion;
pub use geometry::Grid;
pub use model::{EditKind, EditSummary, EditorModel};
pub use select::{extend_from, line_drag, line_range_at, word_drag, word_range_at};

// Re-export the foundation types a caller needs to drive the model, so a
// downstream view can depend on this one crate for the whole editing surface —
// the editing types from the buffer crate, and the highlighting/token/theme
// types a renderer consumes from the syntax crate.
pub use rinch_code_editor_buffer::{
    Bias, Buffer, Change, Edit, History, Position, Selection, Selections,
};
pub use rinch_code_editor_syntax::{
    ColorId, FontStyle, Highlighter, LineTokens, PlainHighlighter, Rgba, Style, Theme,
    TokenMetadata, TokenSpan,
};

// The tree-sitter surface — the concrete highlighter, the bundled-language
// registry, and its error type — is forwarded only when a `lang-*` feature is
// on, mirroring the syntax crate's own gating. A view crate can then build a
// real highlighter through this one dependency without naming the syntax crate.
#[cfg(feature = "tree-sitter")]
pub use rinch_code_editor_syntax::{LanguageConfig, SyntaxError, TreeSitterHighlighter, languages};
