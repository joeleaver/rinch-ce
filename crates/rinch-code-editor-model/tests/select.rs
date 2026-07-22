//! Behavioural tests for the mouse selection gestures — what double and
//! triple clicks select, and how word- and line-mode drags grow. Ported
//! Monaco semantics (see the select module docs); these double as the spec
//! for the view layer's multi-click handling.

use rinch_code_editor_buffer::Buffer;
use rinch_code_editor_model::{extend_from, line_drag, line_range_at, word_drag, word_range_at};

fn buf(text: &str) -> Buffer {
    Buffer::from_text(text)
}

// ---- double click: the word under the pointer -----------------------------

#[test]
fn word_at_the_middle_of_a_word() {
    assert_eq!(word_range_at(&buf("hello world"), 2), 0..5);
}

#[test]
fn word_at_a_words_end_boundary_still_takes_it() {
    // The end boundary is inclusive for words: a click right after the last
    // character selects the word, not the following space.
    assert_eq!(word_range_at(&buf("hello world"), 5), 0..5);
}

#[test]
fn word_at_a_words_start_boundary_takes_the_following_word() {
    assert_eq!(word_range_at(&buf("hello world"), 6), 6..11);
}

#[test]
fn snake_case_is_one_word() {
    assert_eq!(word_range_at(&buf("foo_bar baz"), 3), 0..7);
}

#[test]
fn a_separator_run_selects_whole() {
    assert_eq!(word_range_at(&buf("a === b"), 3), 2..5);
}

#[test]
fn right_after_a_separator_run_prefers_what_follows() {
    // The separator end boundary is exclusive — Monaco's stricter check.
    assert_eq!(word_range_at(&buf("===x"), 3), 3..4);
}

#[test]
fn a_click_in_the_open_selects_the_whitespace_gap() {
    assert_eq!(word_range_at(&buf("a   b"), 2), 1..4);
}

#[test]
fn a_gap_at_the_line_edges_runs_to_them() {
    assert_eq!(word_range_at(&buf("  a"), 0), 0..2, "leading gap");
    assert_eq!(word_range_at(&buf("a  "), 2), 1..3, "trailing gap");
}

#[test]
fn an_empty_line_selects_nothing() {
    assert_eq!(word_range_at(&buf("x\n\ny"), 2), 2..2);
}

#[test]
fn words_are_unicode() {
    assert_eq!(word_range_at(&buf("αβ γδ"), 1), 0..2);
    assert_eq!(word_range_at(&buf("αβ γδ"), 4), 3..5);
}

// ---- triple click: the whole line -----------------------------------------

#[test]
fn line_range_includes_the_terminator() {
    assert_eq!(line_range_at(&buf("aa\nbb\ncc"), 4), 3..6);
}

#[test]
fn line_range_on_the_last_line_stops_at_the_document_end() {
    assert_eq!(line_range_at(&buf("aa\nbb\ncc"), 7), 6..8);
}

// ---- word-mode drag --------------------------------------------------------

#[test]
fn word_drag_forward_snaps_to_the_far_word_end() {
    // Pointer strictly inside "world": the head jumps to its end, the anchor
    // stays at the original word's start.
    let b = buf("hello world");
    let sel = word_drag(&b, &(0..5), 8);
    assert_eq!((sel.anchor, sel.head), (0, 11));
}

#[test]
fn word_drag_at_a_boundary_uses_the_exact_position() {
    // A pointer exactly on a word's start is not "strictly inside" — no snap.
    let b = buf("hello world");
    let sel = word_drag(&b, &(0..5), 6);
    assert_eq!((sel.anchor, sel.head), (0, 6));
}

#[test]
fn word_drag_backward_reverses_and_keeps_the_original_word() {
    // Dragging back before the original word anchors at its END, so the
    // double-clicked word stays inside the (reversed) selection.
    let b = buf("foo bar baz");
    let sel = word_drag(&b, &(4..7), 1);
    assert_eq!((sel.anchor, sel.head), (7, 0), "snapped to foo's start");
}

#[test]
fn word_drag_back_inside_the_original_restores_it() {
    let b = buf("foo bar baz");
    let sel = word_drag(&b, &(4..7), 5);
    assert_eq!((sel.anchor, sel.head), (4, 7));
}

#[test]
fn word_drag_over_whitespace_does_not_snap() {
    let b = buf("foo bar");
    let sel = word_drag(&b, &(0..3), 4);
    assert_eq!((sel.anchor, sel.head), (0, 4));
}

#[test]
fn word_drag_across_lines_snaps_on_the_drag_line() {
    let b = buf("one\ntwo three");
    let sel = word_drag(&b, &(0..3), 9); // inside "three" (8..13)
    assert_eq!((sel.anchor, sel.head), (0, 13));
}

#[test]
fn word_drag_from_an_empty_initial_keeps_the_anchor() {
    // A shift gesture starts from a collapsed range: the anchor never flips.
    let b = buf("foo bar baz");
    let fwd = word_drag(&b, &(5..5), 9); // inside "baz"
    assert_eq!((fwd.anchor, fwd.head), (5, 11));
    let back = word_drag(&b, &(5..5), 1); // inside "foo"
    assert_eq!((back.anchor, back.head), (5, 0));
}

// ---- line-mode drag --------------------------------------------------------

#[test]
fn line_drag_down_keeps_whole_lines() {
    let b = buf("aa\nbb\ncc\ndd");
    let sel = line_drag(&b, &(3..6), 7); // pointer on "cc"
    assert_eq!((sel.anchor, sel.head), (3, 9));
}

#[test]
fn line_drag_up_reverses_around_the_original_line() {
    let b = buf("aa\nbb\ncc\ndd");
    let sel = line_drag(&b, &(3..6), 1); // pointer on "aa"
    assert_eq!((sel.anchor, sel.head), (6, 0));
}

#[test]
fn line_drag_back_on_the_original_line_restores_it() {
    let b = buf("aa\nbb\ncc\ndd");
    let sel = line_drag(&b, &(3..6), 4);
    assert_eq!((sel.anchor, sel.head), (3, 6));
}

#[test]
fn line_drag_to_the_last_line_stops_at_the_document_end() {
    let b = buf("aa\nbb");
    let sel = line_drag(&b, &(0..3), 4);
    assert_eq!((sel.anchor, sel.head), (0, 5));
}

// ---- pins added after the adversarial review ------------------------------
//
// Each guards a behavior a mutation broke without any test above noticing,
// plus the classifier change (Monaco separators, not "everything non-alnum").

#[test]
fn a_word_end_beats_a_separator_start() {
    // The precedence order: touching prev word wins over the following
    // separator run — the one observable ordering among the four branches.
    assert_eq!(word_range_at(&buf("foo==="), 3), 0..3);
}

#[test]
fn a_separator_runs_start_boundary_takes_the_run() {
    // Branch four's inclusive start: exactly on the run's first char (with a
    // gap before it), the run wins over the whitespace gap.
    assert_eq!(word_range_at(&buf("abc ==="), 4), 4..7);
}

#[test]
fn word_range_at_the_document_end() {
    assert_eq!(word_range_at(&buf("hello"), 5), 0..5, "word end inclusive");
    assert_eq!(
        word_range_at(&buf("foo=="), 5),
        5..5,
        "separator end exclusive with nothing after: the empty gap"
    );
    assert_eq!(word_range_at(&buf(""), 0), 0..0);
}

#[test]
fn emoji_and_combining_marks_are_word_material() {
    // Monaco classifies by its separator list, so anything else non-space is
    // part of the word — a decomposed é or an emoji must not split it.
    assert_eq!(word_range_at(&buf("e\u{301}cole test"), 3), 0..6);
    assert_eq!(word_range_at(&buf("ab\u{1F44D}cd ef"), 1), 0..5);
    assert_eq!(word_range_at(&buf("\u{4F60}\u{597D} world"), 1), 0..2);
}

#[test]
fn word_drag_snaps_in_multibyte_text() {
    let b = buf("αβ\nγδ εζ");
    let fwd = word_drag(&b, &(0..2), 7); // strictly inside "εζ" (6..8)
    assert_eq!((fwd.anchor, fwd.head), (0, 8));
    let b = buf("ηθι κλμ");
    let back = word_drag(&b, &(4..7), 1); // strictly inside "ηθι"
    assert_eq!((back.anchor, back.head), (7, 0));
}

#[test]
fn line_drag_from_an_empty_initial_keeps_the_anchor() {
    // The line-mode mirror of the word test: a shift gesture's collapsed
    // range never flips, and back on its own line the head is the range end.
    let b = buf("aa\nbb\ncc\ndd");
    let same = line_drag(&b, &(4..4), 5);
    assert_eq!((same.anchor, same.head), (4, 4));
    let down = line_drag(&b, &(4..4), 7);
    assert_eq!((down.anchor, down.head), (4, 9));
    let up = line_drag(&b, &(4..4), 1);
    assert_eq!((up.anchor, up.head), (4, 0));
}

#[test]
fn line_drag_with_the_initial_on_the_last_line() {
    let b = buf("aa\nbb");
    let initial = line_range_at(&b, 4);
    assert_eq!(initial, 3..5);
    let same = line_drag(&b, &initial, 4);
    assert_eq!((same.anchor, same.head), (3, 5));
    let at_end = line_drag(&b, &initial, 5);
    assert_eq!((at_end.anchor, at_end.head), (3, 5));
    let up = line_drag(&b, &initial, 1);
    assert_eq!((up.anchor, up.head), (5, 0), "anchored at the document end");
}

#[test]
fn extend_from_flips_around_a_range_and_holds_an_empty_one() {
    // Monaco's _computeSelection, used directly by Shift+click.
    let sel = extend_from(&(4..7), 9);
    assert_eq!((sel.anchor, sel.head), (4, 9), "forward: anchor at start");
    let sel = extend_from(&(4..7), 1);
    assert_eq!((sel.anchor, sel.head), (7, 1), "backward: anchor at end");
    let sel = extend_from(&(4..7), 4);
    assert_eq!((sel.anchor, sel.head), (7, 4), "at the start counts as behind");
    let sel = extend_from(&(5..5), 2);
    assert_eq!((sel.anchor, sel.head), (5, 2), "empty range anchors at itself");
}
