//! Cursor-motion behaviour, driven through the public `EditorModel::move_cursors`
//! API. Also pins the grapheme-aware backspace/delete retrofit.

use rinch_code_editor_model::{
    EditorModel, Grid, Highlighter, Motion, Selection, Selections, Theme,
};
use rinch_code_editor_syntax::PlainHighlighter;

fn model(text: &str) -> EditorModel {
    let theme = Theme::builder().build();
    let hl: Box<dyn Highlighter> = Box::new(PlainHighlighter::new(theme));
    EditorModel::from_text(text, hl)
}

/// 8px cells, 16px lines, 4-column tabs.
fn grid() -> Grid {
    Grid::new(8.0, 16.0, 4)
}

fn heads(m: &EditorModel) -> Vec<usize> {
    m.selections().iter().map(|s| s.head).collect()
}

fn head(m: &EditorModel) -> usize {
    m.selections().primary().head
}

// ---- horizontal ----------------------------------------------------------

#[test]
fn left_and_right_step_one_character() {
    let mut m = model("abc");
    m.set_cursor(1);
    m.move_cursors(&grid(), Motion::Right, false);
    assert_eq!(head(&m), 2);
    m.move_cursors(&grid(), Motion::Left, false);
    assert_eq!(head(&m), 1);
}

#[test]
fn horizontal_motion_crosses_line_boundaries() {
    let mut m = model("ab\ncd");
    m.set_cursor(2); // end of line 0 content
    m.move_cursors(&grid(), Motion::Right, false);
    assert_eq!(head(&m), 3, "over the newline to the start of line 1");
    m.move_cursors(&grid(), Motion::Left, false);
    assert_eq!(head(&m), 2, "and back");
}

#[test]
fn right_steps_over_a_combining_grapheme_as_one() {
    let mut m = model("e\u{0301}x"); // é (e + mark), then x
    m.set_cursor(0);
    m.move_cursors(&grid(), Motion::Right, false);
    assert_eq!(head(&m), 2, "the e and its mark move together");
}

#[test]
fn plain_left_right_collapse_a_selection_to_its_edges() {
    let mut m = model("hello");
    m.set_selections(Selections::single(Selection::new(1, 4)));
    m.move_cursors(&grid(), Motion::Left, false);
    assert_eq!(head(&m), 1, "left collapses to the start");

    m.set_selections(Selections::single(Selection::new(1, 4)));
    m.move_cursors(&grid(), Motion::Right, false);
    assert_eq!(head(&m), 4, "right collapses to the end");
}

#[test]
fn extending_keeps_the_anchor_and_drags_the_head() {
    let mut m = model("hello");
    m.set_cursor(1);
    m.move_cursors(&grid(), Motion::Right, true);
    m.move_cursors(&grid(), Motion::Right, true);
    let s = m.selections().primary();
    assert_eq!((s.anchor, s.head), (1, 3));
    assert!(!s.is_empty());
}

// ---- vertical & goal column ----------------------------------------------

#[test]
fn up_and_down_keep_the_column_on_equal_length_lines() {
    let mut m = model("abcdef\nghijkl");
    m.set_cursor(3); // line 0, col 3
    m.move_cursors(&grid(), Motion::Down(1), false);
    assert_eq!(head(&m), 10, "line 1, col 3");
    m.move_cursors(&grid(), Motion::Up(1), false);
    assert_eq!(head(&m), 3, "back to line 0, col 3");
}

#[test]
fn the_goal_column_survives_a_short_line() {
    // Column 5 on line 0, down through the 2-char line 1, out onto line 2: the
    // caret must remember column 5 rather than sticking at the short line's end.
    let mut m = model("abcdef\nxy\nABCDEF");
    m.set_cursor(5); // line 0, col 5
    m.move_cursors(&grid(), Motion::Down(1), false);
    assert_eq!(
        head(&m),
        9,
        "clamped to the end of the short line (offset 7+2)"
    );
    m.move_cursors(&grid(), Motion::Down(1), false);
    assert_eq!(
        head(&m),
        15,
        "line 2, col 5 — the goal column was remembered"
    );
}

#[test]
fn vertical_motion_tracks_visual_columns_across_tabs() {
    // Line 0 is a tab then "x": visual columns tab=0..4, x at 4, caret after x at
    // visual 5. Moving down must land at visual column 5 on the plain line.
    let mut m = model("\tx\nZZZZZZZZ");
    let line1_start = m.text().find('Z').unwrap();
    m.set_cursor(2); // after the x on line 0
    m.move_cursors(&grid(), Motion::Down(1), false);
    assert_eq!(head(&m), line1_start + 5, "visual column 5 on line 1");
}

#[test]
fn up_on_the_first_line_parks_at_the_document_start() {
    let mut m = model("abc\ndef");
    m.set_cursor(2);
    m.move_cursors(&grid(), Motion::Up(1), false);
    assert_eq!(head(&m), 0);
}

#[test]
fn down_on_the_last_line_parks_at_the_document_end() {
    let mut m = model("abc\ndef");
    m.set_cursor(5); // line 1, col 1
    m.move_cursors(&grid(), Motion::Down(1), false);
    assert_eq!(head(&m), 7, "end of the document");
}

#[test]
fn page_motion_is_a_multi_line_step() {
    let mut m = model("l0\nl1\nl2\nl3\nl4\nl5");
    m.set_cursor(0);
    m.move_cursors(&grid(), Motion::Down(3), false);
    // Each line is "lN" (2 chars) + newline = 3 chars; line 3 starts at offset 9.
    assert_eq!(head(&m), 9, "down three lines");
}

// ---- line boundaries -----------------------------------------------------

#[test]
fn smart_home_toggles_between_indent_and_column_zero() {
    let mut m = model("  ab");
    m.set_cursor(4); // end
    m.move_cursors(&grid(), Motion::LineStart, false);
    assert_eq!(head(&m), 2, "first to the indent");
    m.move_cursors(&grid(), Motion::LineStart, false);
    assert_eq!(head(&m), 0, "then to column 0");
    m.move_cursors(&grid(), Motion::LineStart, false);
    assert_eq!(head(&m), 2, "and back to the indent");
}

#[test]
fn line_end_goes_to_the_end_of_content() {
    let mut m = model("abc\ndef");
    m.set_cursor(0);
    m.move_cursors(&grid(), Motion::LineEnd, false);
    assert_eq!(head(&m), 3, "before the newline, not after it");
}

// ---- word motion ---------------------------------------------------------

#[test]
fn word_right_stops_at_word_ends() {
    let mut m = model("foo bar baz");
    m.set_cursor(0);
    m.move_cursors(&grid(), Motion::WordRight, false);
    assert_eq!(head(&m), 3);
    m.move_cursors(&grid(), Motion::WordRight, false);
    assert_eq!(head(&m), 7);
}

#[test]
fn word_left_stops_at_word_starts() {
    let mut m = model("foo bar baz");
    m.set_cursor(11);
    m.move_cursors(&grid(), Motion::WordLeft, false);
    assert_eq!(head(&m), 8);
    m.move_cursors(&grid(), Motion::WordLeft, false);
    assert_eq!(head(&m), 4);
}

#[test]
fn identifiers_are_one_word_but_punctuation_is_its_own_run() {
    let mut m = model("foo_bar.baz");
    m.set_cursor(0);
    m.move_cursors(&grid(), Motion::WordRight, false);
    assert_eq!(head(&m), 7, "foo_bar is a single word");
    m.move_cursors(&grid(), Motion::WordRight, false);
    assert_eq!(head(&m), 8, "the dot is skipped as its own run");
    m.move_cursors(&grid(), Motion::WordRight, false);
    assert_eq!(head(&m), 11, "then baz");
}

// ---- document ends -------------------------------------------------------

#[test]
fn document_start_and_end() {
    let mut m = model("abc\ndef");
    m.set_cursor(4);
    m.move_cursors(&grid(), Motion::DocStart, false);
    assert_eq!(head(&m), 0);
    m.move_cursors(&grid(), Motion::DocEnd, false);
    assert_eq!(head(&m), 7);
}

// ---- multi-cursor --------------------------------------------------------

#[test]
fn motion_applies_to_every_cursor() {
    let mut m = model("ab\ncd");
    m.set_selections(Selections::new(
        vec![Selection::cursor(0), Selection::cursor(3)],
        0,
    ));
    m.move_cursors(&grid(), Motion::Right, false);
    assert_eq!(heads(&m), vec![1, 4]);
}

#[test]
fn cursors_that_collide_after_moving_merge() {
    // Cursors at 0 and 1: moving both left lands them both on 0, where they must
    // merge into one — else the next edit would apply twice at the same spot.
    let mut m = model("abc");
    m.set_selections(Selections::new(
        vec![Selection::cursor(0), Selection::cursor(1)],
        0,
    ));
    m.move_cursors(&grid(), Motion::Left, false);
    assert_eq!(m.selections().len(), 1);
    assert_eq!(head(&m), 0);
}

// ---- grapheme-aware deletion (the Increment 1 retrofit) ------------------

#[test]
fn backspace_removes_a_crlf_in_one_step() {
    let mut m = model("ab\r\ncd");
    m.set_cursor(4); // start of line 1
    m.backspace();
    assert_eq!(m.text(), "abcd", "the whole \\r\\n goes at once");
    assert_eq!(head(&m), 2);
}

#[test]
fn backspace_removes_a_base_and_its_combining_mark_together() {
    let mut m = model("e\u{0301}x");
    m.set_cursor(2); // after é
    m.backspace();
    assert_eq!(m.text(), "x", "é is one perceived character");
    assert_eq!(head(&m), 0);
}

#[test]
fn delete_forward_removes_a_crlf_in_one_step() {
    let mut m = model("ab\r\ncd");
    m.set_cursor(2); // end of line 0 content
    m.delete_forward();
    assert_eq!(m.text(), "abcd");
    assert_eq!(head(&m), 2);
}
