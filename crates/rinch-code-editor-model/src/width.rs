//! How wide the document is — the horizontal half of the scroll range.
//!
//! The vertical scroll range is arithmetic: every line is [`Grid::line_height`]
//! tall, so the document's height is a multiplication and the spacer is exact
//! from the first frame. Horizontally there is no such shortcut. A line's width
//! depends on its contents, so the only honest answer to "how wide is this
//! document" is a fold over every line — and on a large file that is a scan
//! nobody wants on the render path.
//!
//! # Monaco's answer, which we follow
//!
//! Monaco folds the max over the lines it has *rendered*, and only ever grows
//! it. `ViewLines._updateLineWidths` walks the currently materialized lines,
//! and `_ensureMaxLineWidth` raises the high-water mark and never lowers it;
//! the mark resets only when the whole document happens to be rendered at once,
//! or on a flush. Monaco never scans the document for its longest line.
//!
//! The visible consequence is that the horizontal scrollbar *grows* as you
//! scroll down past a long line, and does not shrink again when you scroll
//! back. That is deliberate in Monaco and it is what this reproduces.
//!
//! It is worth being clear that we could do better and are choosing not to.
//! Monaco folds over rendered lines because measuring an unrendered one means
//! putting it in the DOM; our [`Grid`] gives an exact width for any line without
//! rendering it, so a true document-wide maximum is available for the cost of an
//! O(document) scan on load plus an incremental update per edit. That is a real
//! feature — a stable scrollbar — but it is also a second width index to keep
//! correct across every edit, undo and remote splice, and the project's standing
//! rule is to follow Monaco unless there is a concrete reason not to. Being
//! *able* to deviate is not a reason to.

use std::ops::Range;

use rinch_code_editor_buffer::Buffer;

use crate::geometry::Grid;

/// How wide a range of lines is, and whether the cap had to truncate any of
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentWidth {
    /// Grid cells occupied by the widest line, counting at most `cap`
    /// characters of any one of them.
    pub columns: usize,
    /// Whether at least one line was longer than the cap, and so is drawn
    /// truncated.
    ///
    /// The view needs this because a truncated line does not end at
    /// [`columns`](Self::columns): something has to stand in for the part that
    /// was dropped — Monaco appends a "Show more" chip — and the scrollable
    /// width has to leave room for it, or the one control that recovers the
    /// hidden text sits outside the scroll range.
    pub capped: bool,
}

/// Grid cells occupied by the widest of `lines`, counting at most `cap`
/// characters of any one line.
///
/// `cap` is the caller's `stopRenderingLineAfter`: a line is only as wide as
/// the part that gets drawn, so a document whose longest line is capped must
/// not be scrollable past the cap into blank space.
///
/// Walks the rope directly rather than through
/// [`Buffer::line_text`](rinch_code_editor_buffer::Buffer::line_text), which
/// allocates a `String` per line — this runs over every visible line on every
/// scroll, beside a [`compute_lines`](crate) pass that already walks the same
/// characters.
pub fn max_visual_columns(
    buffer: &Buffer,
    grid: &Grid,
    lines: Range<usize>,
    cap: usize,
) -> DocumentWidth {
    let end = lines.end.min(buffer.len_lines());
    let mut out = DocumentWidth {
        columns: 0,
        capped: false,
    };
    for line in lines.start..end {
        // `line_len` already excludes the terminator, so the rope slice's
        // trailing "\r\n" is never counted — a newline is not a cell.
        let len = buffer.line_len(line);
        let count = len.min(cap);
        out.capped |= len > count;
        out.columns = out
            .columns
            .max(grid.visual_width(buffer.rope().line(line).chars(), count));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> Grid {
        Grid::new(8.0, 16.0, 4)
    }

    #[test]
    fn takes_the_widest_line_in_the_range() {
        let buffer = Buffer::from_text("ab\nabcdef\nabc\n");
        assert_eq!(
            max_visual_columns(&buffer, &grid(), 0..3, 10_000).columns,
            6
        );
    }

    #[test]
    fn only_looks_at_the_range_it_is_given() {
        let buffer = Buffer::from_text("ab\nabcdefghij\nabc\n");
        assert_eq!(
            max_visual_columns(&buffer, &grid(), 0..1, 10_000).columns,
            2,
            "the long line is outside the window"
        );
        assert_eq!(
            max_visual_columns(&buffer, &grid(), 2..3, 10_000).columns,
            3,
            "and still is, from the other side"
        );
    }

    #[test]
    fn a_line_terminator_is_not_a_cell() {
        // Both terminator styles, and a final line with none at all.
        for text in ["abcd\nx", "abcd\r\nx"] {
            assert_eq!(
                max_visual_columns(&Buffer::from_text(text), &grid(), 0..1, 10_000).columns,
                4,
                "in {text:?}"
            );
        }
    }

    #[test]
    fn tabs_expand_to_their_stops() {
        // "\tab" is a 4-cell tab then two chars = 6, wider than the 5 chars of
        // "abcde" even though it is the shorter line.
        let buffer = Buffer::from_text("abcde\n\tab\n");
        assert_eq!(
            max_visual_columns(&buffer, &grid(), 0..2, 10_000).columns,
            6
        );
    }

    #[test]
    fn full_width_characters_count_double() {
        let buffer = Buffer::from_text("abcd\n日本\n");
        assert_eq!(
            max_visual_columns(&buffer, &grid(), 0..2, 10_000).columns,
            4,
            "two CJK glyphs are four cells, tying with four ASCII"
        );
        let buffer = Buffer::from_text("abcd\n日本語\n");
        assert_eq!(
            max_visual_columns(&buffer, &grid(), 0..2, 10_000).columns,
            6
        );
    }

    #[test]
    fn the_cap_bounds_the_width() {
        let buffer = Buffer::from_text(&"x".repeat(500));
        assert_eq!(max_visual_columns(&buffer, &grid(), 0..1, 100).columns, 100);
        assert_eq!(
            max_visual_columns(&buffer, &grid(), 0..1, 10_000).columns,
            500,
            "a cap above the line length does not truncate"
        );
    }

    /// The cap counts *characters*, so a capped line of full-width glyphs is
    /// twice as wide as a capped line of ASCII. That is correct — the cap is
    /// how much text is drawn, not how much room it takes.
    #[test]
    fn the_cap_counts_characters_not_cells() {
        let buffer = Buffer::from_text(&"日".repeat(500));
        assert_eq!(max_visual_columns(&buffer, &grid(), 0..1, 100).columns, 200);
    }

    #[test]
    fn capped_reports_whether_anything_was_truncated() {
        let buffer = Buffer::from_text("short\nalso short\n");
        assert!(!max_visual_columns(&buffer, &grid(), 0..2, 100).capped);

        let long = Buffer::from_text(&format!("short\n{}\n", "x".repeat(500)));
        assert!(
            max_visual_columns(&long, &grid(), 0..2, 100).capped,
            "the second line overruns"
        );
        assert!(
            !max_visual_columns(&long, &grid(), 0..1, 100).capped,
            "…but it is outside this window"
        );
        assert!(
            !max_visual_columns(&long, &grid(), 0..2, 10_000).capped,
            "…and a cap it fits under does not truncate it"
        );
    }

    /// A line of exactly `cap` characters is drawn whole, so nothing stands in
    /// for a missing tail and the flag must stay clear. The off-by-one here is
    /// the difference between a stray chip on an untruncated line and none.
    #[test]
    fn a_line_exactly_at_the_cap_is_not_capped() {
        let exact = Buffer::from_text(&"x".repeat(100));
        let w = max_visual_columns(&exact, &grid(), 0..1, 100);
        assert_eq!(w.columns, 100);
        assert!(!w.capped);

        let one_over = Buffer::from_text(&"x".repeat(101));
        assert!(max_visual_columns(&one_over, &grid(), 0..1, 100).capped);
    }

    #[test]
    fn an_empty_range_or_document_is_zero_wide() {
        let buffer = Buffer::from_text("abcdef\n");
        assert_eq!(
            max_visual_columns(&buffer, &grid(), 0..0, 10_000).columns,
            0
        );
        assert_eq!(
            max_visual_columns(&Buffer::new(), &grid(), 0..1, 10_000).columns,
            0
        );
    }

    #[test]
    fn a_range_past_the_end_is_clamped_not_panicking() {
        let buffer = Buffer::from_text("ab\ncdef\n");
        assert_eq!(
            max_visual_columns(&buffer, &grid(), 0..999, 10_000).columns,
            4,
            "the range clamps to the document"
        );
        assert_eq!(
            max_visual_columns(&buffer, &grid(), 999..1000, 10_000).columns,
            0
        );
    }
}
