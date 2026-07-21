//! The highlighting seam.
//!
//! Highlighters are asked for tokens **a viewport at a time**, not a document at
//! a time. That is the single most important thing about this interface: it is
//! what allows a 100 MB file to open instantly, because only the ~50 visible
//! lines are ever tokenized. Monaco does the same thing, tokenizing ahead of the
//! viewport in background chunks rather than up front.

use crate::line_tokens::LineTokens;
use crate::theme::Theme;
use rinch_code_editor_buffer::{Buffer, Change};
use std::ops::Range;

/// Produces [`LineTokens`] for ranges of lines.
///
/// Implementations may cache aggressively, which is why every method takes
/// `&mut self` — an incremental parser mutates its tree on each edit, and even a
/// read of tokens may populate a cache.
pub trait Highlighter {
    /// Discard all state and re-establish it from `buffer`. Called on load, on
    /// language change, and whenever an edit arrives that the highlighter cannot
    /// apply incrementally.
    fn reset(&mut self, buffer: &Buffer);

    /// Inform the highlighter of a change already applied to `buffer`.
    ///
    /// `buffer` is in its **post-change** state. Implementations that cannot
    /// apply the change incrementally should fall back to [`Highlighter::reset`].
    fn apply_change(&mut self, buffer: &Buffer, change: &Change);

    /// Append tokens for each line in `lines` to `out`, one [`LineTokens`] per
    /// line, in order. Lines past the end of the buffer contribute nothing, so
    /// the caller must not assume `out.len() == lines.len()`.
    ///
    /// `out` is passed in rather than returned so the renderer can reuse one
    /// allocation across frames.
    fn tokens(&mut self, buffer: &Buffer, lines: Range<usize>, out: &mut Vec<LineTokens>);

    /// The theme these tokens' [`ColorId`](crate::ColorId)s index into.
    fn theme(&self) -> &Theme;

    /// Convenience wrapper over [`Highlighter::tokens`] for a single line.
    fn line_tokens(&mut self, buffer: &Buffer, line: usize) -> LineTokens {
        let mut out = Vec::with_capacity(1);
        self.tokens(buffer, line..line + 1, &mut out);
        out.pop().unwrap_or_default()
    }
}

/// A highlighter that applies the theme's default style to everything.
///
/// This is the fallback for files with no known language, and it is what makes
/// the editor usable with no syntax backend compiled in at all.
#[derive(Debug, Clone)]
pub struct PlainHighlighter {
    theme: Theme,
}

impl PlainHighlighter {
    pub fn new(theme: Theme) -> Self {
        Self { theme }
    }
}

impl Highlighter for PlainHighlighter {
    fn reset(&mut self, _buffer: &Buffer) {}

    fn apply_change(&mut self, _buffer: &Buffer, _change: &Change) {}

    fn tokens(&mut self, buffer: &Buffer, lines: Range<usize>, out: &mut Vec<LineTokens>) {
        let default = self.theme.default_metadata();
        let end = lines.end.min(buffer.len_lines());
        for line in lines.start..end {
            let len = buffer.line_len(line) as u32;
            if len == 0 {
                out.push(LineTokens::empty());
            } else {
                out.push(LineTokens::single(len, default));
            }
        }
    }

    fn theme(&self) -> &Theme {
        &self.theme
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Rgba, Style};
    use rinch_code_editor_buffer::Edit;

    fn plain() -> PlainHighlighter {
        PlainHighlighter::new(
            Theme::builder()
                .default_foreground(Rgba::rgb(0xAB, 0xB2, 0xBF))
                .rule("keyword", Style::fg(Rgba::rgb(1, 2, 3)))
                .build(),
        )
    }

    #[test]
    fn every_line_becomes_one_default_token() {
        let buffer = Buffer::from_text("alpha\nbeta\ngamma");
        let mut h = plain();
        let mut out = Vec::new();
        h.tokens(&buffer, 0..3, &mut out);

        assert_eq!(out.len(), 3);
        assert_eq!(out[0].count(), 1);
        assert_eq!(out[0].line_len(), 5);
        assert_eq!(out[1].line_len(), 4);
        assert_eq!(out[2].line_len(), 5);
        assert_eq!(out[0].metadata_at(0), Some(h.theme().default_metadata()));
    }

    #[test]
    fn empty_lines_produce_no_tokens() {
        let buffer = Buffer::from_text("a\n\nb");
        let mut h = plain();
        let mut out = Vec::new();
        h.tokens(&buffer, 0..3, &mut out);
        assert_eq!(out[1].count(), 0, "an empty line has nothing to style");
        assert!(out[1].is_empty());
    }

    #[test]
    fn a_range_past_the_end_is_clamped_rather_than_panicking() {
        let buffer = Buffer::from_text("only one line");
        let mut h = plain();
        let mut out = Vec::new();
        h.tokens(&buffer, 0..100, &mut out);
        assert_eq!(
            out.len(),
            1,
            "caller must not assume out.len() == lines.len()"
        );
    }

    #[test]
    fn tokens_are_requested_by_viewport_not_whole_document() {
        // The point of the interface: asking for lines 2..4 of a 1000-line file
        // yields exactly two results and touches nothing else.
        let text = (0..1000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let buffer = Buffer::from_text(&text);
        let mut h = plain();
        let mut out = Vec::new();
        h.tokens(&buffer, 2..4, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].line_len(), "line 2".len() as u32);
        assert_eq!(out[1].line_len(), "line 3".len() as u32);
    }

    #[test]
    fn out_is_appended_to_not_cleared() {
        let buffer = Buffer::from_text("a\nb");
        let mut h = plain();
        let mut out = vec![LineTokens::empty()];
        h.tokens(&buffer, 0..2, &mut out);
        assert_eq!(out.len(), 3, "existing entries are preserved");
    }

    #[test]
    fn line_tokens_convenience_matches_the_range_call() {
        let buffer = Buffer::from_text("alpha\nbeta");
        let mut h = plain();
        let single = h.line_tokens(&buffer, 1);
        let mut out = Vec::new();
        h.tokens(&buffer, 1..2, &mut out);
        assert_eq!(single, out[0]);
    }

    #[test]
    fn plain_highlighter_ignores_changes_without_error() {
        let mut buffer = Buffer::from_text("abc");
        let mut h = plain();
        let change = Change::apply(&mut buffer, vec![Edit::insert(0, "x")]);
        h.apply_change(&buffer, &change);
        h.reset(&buffer);
        assert_eq!(h.line_tokens(&buffer, 0).line_len(), 4);
    }
}
