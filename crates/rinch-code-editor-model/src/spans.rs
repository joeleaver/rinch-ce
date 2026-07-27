//! Where a selection lands on the grid, one line at a time.
//!
//! A selection is a pair of character offsets, but it is *drawn* as one
//! rectangle per line it covers. Splitting the first into the second is the same
//! arithmetic whether the selection belongs to the person typing or to a
//! collaborator on the other side of a network, so it lives here — rinch-free,
//! beside the [`Grid`] it measures against, and unit-tested — rather than in the
//! view layer where only one of the two callers could reach it.
//!
//! Monaco's counterpart is `LineVisibleRanges`/`HorizontalRange`, which carries
//! pixels. [`LineSpan`] carries *visual columns* instead, and the view multiplies
//! in the measured cell advance at the last moment. That is the same division of
//! labour as the rest of this crate: columns are exact and testable without a
//! font, pixels are not.

use std::ops::Range;

use rinch_code_editor_buffer::{Buffer, Selection};

use crate::geometry::Grid;

/// One line's worth of a selection, in visual columns. `end_vcol` is exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineSpan {
    pub line: usize,
    pub start_vcol: usize,
    pub end_vcol: usize,
}

impl LineSpan {
    /// Width in visual columns. Never negative: the constructor below only emits
    /// spans whose end is strictly past their start.
    pub fn width(&self) -> usize {
        self.end_vcol - self.start_vcol
    }
}

/// Split `selection` into the rectangles that draw it, clipped to the `visible`
/// line window.
///
/// Clipping is not an optimization detail: a select-all on a large file would
/// otherwise emit one rectangle per line of the document, every frame, for rows
/// that are not materialized.
///
/// Two rules are worth naming, because both are invisible until they are wrong:
///
/// - **The line-break sliver.** A line whose terminator falls inside the
///   selection gets one extra column past its content, so a selected newline
///   reads as selected instead of the selection appearing to stop short.
/// - **Zero-width spans are dropped.** A selection ending at column 0 of a line
///   covers none of it. Emitting the empty rectangle anyway would paint nothing
///   while still costing a DOM node — and a zero-dimension node is the exact
///   shape rinch's paint fast path mishandles (joeleaver/rinch#142).
pub fn selection_spans(
    buffer: &Buffer,
    grid: &Grid,
    selection: Selection,
    visible: Range<usize>,
) -> Vec<LineSpan> {
    // An empty window means nothing is on screen. Without this the clamp below
    // (`visible.end - 1`) would turn `0..0` into the single line 0 and draw a
    // rectangle into a viewport that is showing no lines at all.
    if selection.is_empty() || visible.is_empty() {
        return Vec::new();
    }

    let len = buffer.len_chars();
    let start = buffer.position_of(selection.start().min(len));
    let end = buffer.position_of(selection.end().min(len));

    let first = start.line.max(visible.start);
    let last = end.line.min(visible.end - 1);
    // An inclusive range whose start is past its end is empty, not a panic, so
    // a selection entirely outside the window falls out here on its own.
    let mut out = Vec::with_capacity(last.saturating_sub(first) + 1);
    for line in first..=last {
        let line_text = buffer.line_text(line);
        let s_col = if line == start.line { start.col } else { 0 };
        let e_col = if line == end.line {
            end.col
        } else {
            buffer.line_len(line)
        };
        let start_vcol = grid.visual_column(&line_text, s_col);
        let mut end_vcol = grid.visual_column(&line_text, e_col);
        if line < end.line {
            end_vcol += 1;
        }
        if end_vcol > start_vcol {
            out.push(LineSpan {
                line,
                start_vcol,
                end_vcol,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAB_SIZE: usize = 4;

    fn grid() -> Grid {
        Grid::new(1.0, 20.0, TAB_SIZE)
    }

    fn spans(text: &str, sel: Selection, visible: Range<usize>) -> Vec<LineSpan> {
        selection_spans(&Buffer::from_text(text), &grid(), sel, visible)
    }

    #[test]
    fn a_selection_within_one_line_is_one_span() {
        let got = spans("hello world", Selection::new(2, 7), 0..1);
        assert_eq!(
            got,
            vec![LineSpan {
                line: 0,
                start_vcol: 2,
                end_vcol: 7
            }]
        );
        assert_eq!(got[0].width(), 5);
    }

    #[test]
    fn an_empty_selection_draws_nothing() {
        assert!(spans("hello", Selection::cursor(3), 0..1).is_empty());
    }

    #[test]
    fn a_span_is_the_same_whichever_way_the_selection_points() {
        assert_eq!(
            spans("hello world", Selection::new(7, 2), 0..1),
            spans("hello world", Selection::new(2, 7), 0..1),
        );
    }

    /// Every line but the last carries one extra column, so the newline the
    /// selection swallowed is visible as swallowed.
    #[test]
    fn a_selected_line_break_shows_as_a_one_column_sliver() {
        // "ab\ncd\nef": select from the middle of line 0 to the middle of line 2.
        let got = spans("ab\ncd\nef", Selection::new(1, 7), 0..3);
        assert_eq!(
            got,
            vec![
                // "ab" is 2 columns of content plus the terminator sliver.
                LineSpan {
                    line: 0,
                    start_vcol: 1,
                    end_vcol: 3
                },
                LineSpan {
                    line: 1,
                    start_vcol: 0,
                    end_vcol: 3
                },
                LineSpan {
                    line: 2,
                    start_vcol: 0,
                    end_vcol: 1
                },
            ]
        );
    }

    #[test]
    fn spans_are_clipped_to_the_visible_window() {
        let text = "a\nb\nc\nd\ne\nf\ng\n";
        let all = spans(text, Selection::new(0, 13), 0..8);
        assert!(all.len() > 3);

        let clipped = spans(text, Selection::new(0, 13), 2..5);
        assert_eq!(
            clipped.iter().map(|s| s.line).collect::<Vec<_>>(),
            vec![2, 3, 4],
            "only the window's lines are emitted"
        );
    }

    #[test]
    fn a_selection_entirely_outside_the_window_draws_nothing() {
        let text = "a\nb\nc\nd\ne\nf\n";
        assert!(spans(text, Selection::new(0, 3), 4..6).is_empty());
        assert!(spans(text, Selection::new(8, 11), 0..2).is_empty());
    }

    /// `0..0` is what an unlaid-out viewport reports. Clamping `end - 1` without
    /// checking would turn it into "line 0 is visible" and draw into a viewport
    /// showing nothing.
    #[test]
    fn an_empty_window_draws_nothing() {
        assert!(spans("hello", Selection::new(0, 5), 0..0).is_empty());
    }

    /// A selection ending at column 0 covers none of that line. The rectangle
    /// would be invisible either way; not emitting it keeps a zero-dimension
    /// node out of rinch's paint fast path.
    #[test]
    fn a_zero_width_span_is_dropped() {
        // "ab\ncd": select "ab\n" exactly — line 1 is entered but not covered.
        let got = spans("ab\ncd", Selection::new(0, 3), 0..2);
        assert_eq!(
            got,
            vec![LineSpan {
                line: 0,
                start_vcol: 0,
                end_vcol: 3
            }],
            "line 1 contributes nothing and must not appear"
        );
    }

    #[test]
    fn tabs_are_measured_in_grid_columns() {
        // A tab advances to the next multiple of TAB_SIZE, so selecting "\tx"
        // spans 5 columns, not 2.
        let got = spans("\tx", Selection::new(0, 2), 0..1);
        assert_eq!(
            got,
            vec![LineSpan {
                line: 0,
                start_vcol: 0,
                end_vcol: TAB_SIZE + 1
            }]
        );
    }

    /// Offsets can outlive the text they named — a collaborator's caret resolved
    /// against a document this buffer has not caught up with. Clamping keeps
    /// that drawable instead of panicking inside a renderer.
    #[test]
    fn offsets_past_the_end_are_clamped_rather_than_panicking() {
        let got = spans("abc", Selection::new(1, 999), 0..1);
        assert_eq!(
            got,
            vec![LineSpan {
                line: 0,
                start_vcol: 1,
                end_vcol: 3
            }]
        );
    }
}
