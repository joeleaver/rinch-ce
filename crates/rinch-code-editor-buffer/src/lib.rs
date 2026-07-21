//! Rope-backed text buffer and multi-cursor editing model for the rinch code editor.
//!
//! This crate is deliberately free of any dependency on rinch, on a renderer, or
//! on a clock. It holds the parts that are easy to get subtly wrong — multi-cursor
//! edit composition, position rebasing, and exact undo — so they can be tested and
//! fuzzed without a GUI in the picture.
//!
//! # Offsets are chars, never bytes
//!
//! Every offset in this crate is a **char (Unicode scalar) offset**, matching
//! [`ropey`]'s primary index space. Two things fall out of that choice:
//!
//! - Automerge's `Text` is codepoint-indexed, so the collaborative projection maps
//!   across with no conversion at all.
//! - Slicing can never split a codepoint, so a whole class of panic disappears.
//!
//! Byte offsets appear only at the boundary where text layout demands them.
//!
//! # No clock
//!
//! Undo coalescing is *caller-driven* ([`History::push`] takes a `coalesce` flag)
//! rather than time-windowed internally. `std::time::Instant` panics on
//! `wasm32-unknown-unknown`, and rather than pull in a time shim, the view layer —
//! which already has a frame clock — decides when two edits belong together.

mod buffer;
mod edit;
mod history;
mod selection;

pub use buffer::{Buffer, Position};
pub use edit::{Bias, Change, Edit};
pub use history::History;
pub use selection::{Selection, Selections};
