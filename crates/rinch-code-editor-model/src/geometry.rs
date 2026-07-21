use std::ops::Range;

use unicode_width::UnicodeWidthChar;

/// The monospace grid coordinate system: how a model position (line, char
/// column) maps to and from pixels.
///
/// Three coordinate spaces, and the whole module exists to convert between them:
///
/// - **char column** — an index into a line's chars. This is what
///   [`Position::col`](rinch_code_editor_buffer::Position) holds and what the
///   buffer speaks.
/// - **visual column** — grid cells from the line start, once tabs are expanded
///   to their stops and full-width characters count as two cells. A tab is not
///   one column wide, so char column and visual column diverge on any line that
///   contains one.
/// - **pixel** — visual column times `cell_width` for x, line times
///   `line_height` for y.
///
/// # Why a grid at all
///
/// rinch exposes no point→offset hit-test on its portable document trait, and
/// doing caret math by measuring glyphs would diverge between the desktop
/// (Parley) and web (DOM range) backends. A monospace grid sidesteps both: it is
/// exact for a monospace font, and it is the same fast path Monaco uses.
///
/// Its limits are honest. Ligatures and proportional fallback fonts render at
/// widths this arithmetic does not know; reconciling those needs the runtime
/// measurement path (a later increment), not the grid. What the grid *does*
/// model correctly is the common code case: fixed-advance glyphs, tabs, and
/// full-width CJK.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid {
    cell_width: f32,
    line_height: f32,
    tab_size: usize,
}

impl Grid {
    /// `cell_width` and `line_height` are pixels, measured once per backend at
    /// runtime (they are not bit-identical across desktop and web). `tab_size` is
    /// grid columns per tab stop and is clamped to at least 1.
    pub fn new(cell_width: f32, line_height: f32, tab_size: usize) -> Self {
        Self {
            cell_width,
            line_height,
            tab_size: tab_size.max(1),
        }
    }

    pub fn cell_width(&self) -> f32 {
        self.cell_width
    }

    pub fn line_height(&self) -> f32 {
        self.line_height
    }

    pub fn tab_size(&self) -> usize {
        self.tab_size
    }

    // ---- char column  →  visual column  →  pixel -------------------------

    /// Visual column (grid cells from the line start) at `char_col`. Walking the
    /// line is unavoidable: a tab's width depends on how many cells precede it,
    /// so there is no closed form. `char_col` past the line end clamps to the end.
    pub fn visual_column(&self, line_text: &str, char_col: usize) -> usize {
        let mut vcol = 0;
        for ch in line_text.chars().take(char_col) {
            vcol += self.cell_advance(ch, vcol);
        }
        vcol
    }

    /// Pixel x of the left edge of the cell at `char_col`.
    pub fn x_of(&self, line_text: &str, char_col: usize) -> f32 {
        self.visual_column(line_text, char_col) as f32 * self.cell_width
    }

    /// Pixel y of the top edge of `line`.
    pub fn y_of(&self, line: usize) -> f32 {
        line as f32 * self.line_height
    }

    // ---- pixel  →  position (hit-testing) --------------------------------

    /// The char column whose boundary is nearest `target` visual columns from the
    /// line start — i.e. click-to-place-caret. The caret snaps to whichever
    /// boundary, before or after a cell, the point is closer to, exactly as
    /// Monaco does. A zero-width character (a combining mark) has no midpoint and
    /// is never itself a landing boundary.
    pub fn char_column_at_visual(&self, line_text: &str, target: f32) -> usize {
        if target <= 0.0 {
            return 0;
        }
        let mut vcol = 0usize;
        for (i, ch) in line_text.chars().enumerate() {
            let adv = self.cell_advance(ch, vcol);
            if adv > 0 && target < vcol as f32 + adv as f32 / 2.0 {
                return i;
            }
            vcol += adv;
        }
        line_text.chars().count()
    }

    /// The char column nearest pixel `x` on `line_text`.
    pub fn char_col_at_x(&self, line_text: &str, x: f32) -> usize {
        if self.cell_width <= 0.0 {
            return 0;
        }
        self.char_column_at_visual(line_text, x / self.cell_width)
    }

    /// The model line containing pixel `y`, clamped to the last line. `y` above
    /// the first line yields line 0.
    pub fn line_at_y(&self, y: f32, total_lines: usize) -> usize {
        if total_lines == 0 || self.line_height <= 0.0 {
            return 0;
        }
        let line = (y / self.line_height).floor().max(0.0) as usize;
        line.min(total_lines - 1)
    }

    // ---- viewport --------------------------------------------------------

    /// The model lines that intersect the viewport `[scroll_top, scroll_top +
    /// viewport_height)`, widened by `overscan` lines on each side and clamped to
    /// the document. This is the slice the view materializes; everything outside
    /// it is a spacer.
    pub fn visible_lines(
        &self,
        scroll_top: f32,
        viewport_height: f32,
        total_lines: usize,
        overscan: usize,
    ) -> Range<usize> {
        if total_lines == 0 || self.line_height <= 0.0 {
            return 0..0;
        }
        let first = (scroll_top / self.line_height).floor().max(0.0) as usize;
        let last = ((scroll_top + viewport_height) / self.line_height)
            .ceil()
            .max(0.0) as usize;
        let start = first.saturating_sub(overscan).min(total_lines);
        let end = last.saturating_add(overscan).min(total_lines);
        start.min(end)..end
    }

    /// Total pixel height of the document — the height the virtualization spacer
    /// must have so the scrollbar reflects the whole document, not just the
    /// materialized slice.
    pub fn content_height(&self, total_lines: usize) -> f32 {
        total_lines as f32 * self.line_height
    }

    // ---- internal --------------------------------------------------------

    /// Grid cells a character occupies, given how many cells already precede it
    /// on the line (which only a tab cares about).
    fn cell_advance(&self, ch: char, at_vcol: usize) -> usize {
        if ch == '\t' {
            self.tab_size - (at_vcol % self.tab_size)
        } else {
            char_cells(ch)
        }
    }
}

/// Display width of a single non-tab character: 0 for a combining mark or control
/// character that adds no advance, 2 for a full-width (CJK) glyph, 1 otherwise.
fn char_cells(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 8px cells, 16px lines, 4-column tabs — round numbers to keep the pixel
    // arithmetic obvious.
    fn grid() -> Grid {
        Grid::new(8.0, 16.0, 4)
    }

    #[test]
    fn plain_ascii_is_one_cell_per_char() {
        let g = grid();
        assert_eq!(g.visual_column("abcd", 3), 3);
        assert_eq!(g.x_of("abcd", 3), 24.0);
        assert_eq!(g.y_of(2), 32.0);
    }

    #[test]
    fn char_column_past_the_end_clamps() {
        assert_eq!(grid().visual_column("ab", 9), 2);
    }

    #[test]
    fn a_leading_tab_advances_to_the_stop() {
        let g = grid();
        // "\tab": the tab fills columns 0..4, then a, b.
        assert_eq!(g.visual_column("\tab", 1), 4);
        assert_eq!(g.visual_column("\tab", 2), 5);
        assert_eq!(g.x_of("\tab", 1), 32.0);
    }

    #[test]
    fn a_tab_mid_line_fills_only_to_the_next_stop() {
        let g = grid();
        // "a\tb": a=col0, tab at vcol 1 advances 3 to reach 4, b at 4.
        assert_eq!(g.visual_column("a\tb", 2), 4);
        assert_eq!(g.visual_column("a\tb", 3), 5);
    }

    #[test]
    fn a_tab_landing_on_a_stop_still_advances_a_full_tab() {
        let g = grid();
        // "abcd\t": four chars fill 0..4, the tab then spans a whole 4 to reach 8.
        assert_eq!(g.visual_column("abcd\t", 5), 8);
    }

    #[test]
    fn full_width_characters_are_two_cells() {
        let g = grid();
        assert_eq!(g.visual_column("日本", 1), 2);
        assert_eq!(g.visual_column("日本", 2), 4);
        assert_eq!(g.x_of("日本語", 3), 48.0);
    }

    #[test]
    fn combining_marks_add_no_width() {
        let g = grid();
        // 'e' + combining acute renders in one cell.
        let s = "e\u{0301}x";
        assert_eq!(g.visual_column(s, 2), 1, "the mark stacks on the e");
        assert_eq!(g.visual_column(s, 3), 2, "then x takes the next cell");
    }

    #[test]
    fn hit_test_snaps_to_the_nearest_boundary() {
        let g = grid(); // 8px cells
        // Column 2 spans px 16..24, midpoint 20.
        assert_eq!(g.char_col_at_x("abcd", 18.0), 2, "before the midpoint");
        assert_eq!(g.char_col_at_x("abcd", 22.0), 3, "past the midpoint");
        assert_eq!(g.char_col_at_x("abcd", 16.0), 2, "exactly on the boundary");
    }

    #[test]
    fn hit_test_clamps_outside_the_line() {
        let g = grid();
        assert_eq!(g.char_col_at_x("abcd", -5.0), 0, "left of the line");
        assert_eq!(g.char_col_at_x("abcd", 999.0), 4, "right of the line → end");
    }

    #[test]
    fn hit_test_through_a_tab_uses_its_full_width() {
        let g = grid();
        // "\tab": the tab spans px 0..32, midpoint 16. Clicking at 20 (past the
        // midpoint) lands after the tab, at column 1.
        assert_eq!(g.char_col_at_x("\tab", 20.0), 1);
        assert_eq!(
            g.char_col_at_x("\tab", 10.0),
            0,
            "before the tab's midpoint"
        );
    }

    #[test]
    fn hit_test_and_x_of_round_trip_on_plain_text() {
        let g = grid();
        for col in 0..=4 {
            let x = g.x_of("abcd", col);
            assert_eq!(g.char_col_at_x("abcd", x), col);
        }
    }

    #[test]
    fn line_at_y_floors_and_clamps() {
        let g = grid(); // 16px lines
        assert_eq!(g.line_at_y(0.0, 10), 0);
        assert_eq!(g.line_at_y(31.0, 10), 1, "31px is still within line 1");
        assert_eq!(g.line_at_y(32.0, 10), 2);
        assert_eq!(g.line_at_y(-4.0, 10), 0, "above the top clamps to 0");
        assert_eq!(
            g.line_at_y(9999.0, 10),
            9,
            "below the bottom clamps to last"
        );
    }

    #[test]
    fn visible_lines_covers_the_viewport() {
        let g = grid(); // 16px lines
        // 160px viewport at the top shows exactly lines 0..10.
        assert_eq!(g.visible_lines(0.0, 160.0, 50, 0), 0..10);
        // Scrolled down 160px: lines 10..20.
        assert_eq!(g.visible_lines(160.0, 160.0, 50, 0), 10..20);
    }

    #[test]
    fn visible_lines_applies_overscan_and_clamps_to_the_document() {
        let g = grid();
        assert_eq!(
            g.visible_lines(0.0, 160.0, 50, 3),
            0..13,
            "no overscan below 0"
        );
        assert_eq!(g.visible_lines(160.0, 160.0, 50, 3), 7..23);
        // A short document clamps the end.
        assert_eq!(g.visible_lines(0.0, 160.0, 5, 0), 0..5);
    }

    #[test]
    fn visible_lines_past_the_end_is_empty_not_inverted() {
        let g = grid();
        let r = g.visible_lines(10_000.0, 160.0, 5, 0);
        assert!(r.start <= r.end, "range must never invert");
        assert_eq!(r, 5..5);
    }

    #[test]
    fn an_empty_document_has_no_visible_lines() {
        assert_eq!(grid().visible_lines(0.0, 160.0, 0, 2), 0..0);
    }

    #[test]
    fn content_height_spans_every_line() {
        assert_eq!(grid().content_height(50), 800.0);
        assert_eq!(grid().content_height(0), 0.0);
    }

    #[test]
    fn tab_size_is_clamped_to_at_least_one() {
        let g = Grid::new(8.0, 16.0, 0);
        assert_eq!(g.tab_size(), 1);
        assert_eq!(g.visual_column("\t\t", 2), 2, "each tab advances one cell");
    }

    #[test]
    fn a_degenerate_cell_width_does_not_divide_by_zero() {
        let g = Grid::new(0.0, 16.0, 4);
        assert_eq!(g.char_col_at_x("abc", 10.0), 0);
    }
}
