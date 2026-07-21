//! Behavioural tests for the orchestration. These drive only the public API —
//! the same surface the view layer will use — so they double as its spec.

use rinch_code_editor_model::{EditKind, EditorModel, Highlighter, Selection, Selections, Theme};
use rinch_code_editor_syntax::PlainHighlighter;

/// A model over `text`, highlighted by the trivial plain highlighter (one token
/// per line, no grammar). Nothing here depends on tree-sitter.
fn model(text: &str) -> EditorModel {
    let theme = Theme::builder().build();
    let hl: Box<dyn Highlighter> = Box::new(PlainHighlighter::new(theme));
    EditorModel::from_text(text, hl)
}

fn empty() -> EditorModel {
    let theme = Theme::builder().build();
    let hl: Box<dyn Highlighter> = Box::new(PlainHighlighter::new(theme));
    EditorModel::new(hl)
}

fn carets(m: &EditorModel) -> Vec<usize> {
    m.selections().iter().map(|s| s.head).collect()
}

// ---- basic editing -------------------------------------------------------

#[test]
fn insert_at_a_cursor_advances_the_caret() {
    let mut m = model("hello world");
    m.set_cursor(5);
    m.insert(",");
    assert_eq!(m.text(), "hello, world");
    assert_eq!(carets(&m), vec![6]);
    assert!(m.selections().primary().is_empty());
}

#[test]
fn typing_over_a_selection_replaces_it_and_collapses_the_caret() {
    // The subtlety the model exists to get right: after typing over selected
    // text the caret is a collapsed cursor at the end of what was typed, NOT a
    // selection covering the new text.
    let mut m = model("hello");
    m.set_selections(Selections::single(Selection::new(0, 5)));
    m.insert("x");
    assert_eq!(m.text(), "x");
    assert_eq!(m.selections().len(), 1);
    assert!(m.selections().primary().is_empty());
    assert_eq!(carets(&m), vec![1]);
}

#[test]
fn multi_cursor_insert_lands_and_leaves_a_caret_at_each() {
    let mut m = model("abc");
    m.set_selections(Selections::new(
        vec![
            Selection::cursor(0),
            Selection::cursor(1),
            Selection::cursor(3),
        ],
        0,
    ));
    m.insert("x");
    assert_eq!(m.text(), "xaxbcx");
    assert_eq!(carets(&m), vec![1, 3, 6]);
    assert!(m.selections().iter().all(|s| s.is_empty()));
}

#[test]
fn multi_cursor_typing_over_selections_of_differing_widths_stays_aligned() {
    let mut m = model("aa\nbb\ncc");
    m.set_selections(Selections::new(
        vec![
            Selection::new(0, 2),
            Selection::new(3, 5),
            Selection::new(6, 8),
        ],
        0,
    ));
    m.insert(">");
    assert_eq!(m.text(), ">\n>\n>");
    assert_eq!(carets(&m), vec![1, 3, 5]);
    assert!(m.selections().iter().all(|s| s.is_empty()));
}

#[test]
fn backspace_removes_the_character_before_the_cursor() {
    let mut m = model("abc");
    m.set_cursor(2);
    m.backspace();
    assert_eq!(m.text(), "ac");
    assert_eq!(carets(&m), vec![1]);
}

#[test]
fn backspace_over_a_selection_deletes_the_range() {
    let mut m = model("hello world");
    m.set_selections(Selections::single(Selection::new(5, 11)));
    m.backspace();
    assert_eq!(m.text(), "hello");
    assert_eq!(carets(&m), vec![5]);
}

#[test]
fn delete_forward_removes_the_character_after_the_cursor() {
    let mut m = model("abc");
    m.set_cursor(1);
    m.delete_forward();
    assert_eq!(m.text(), "ac");
    assert_eq!(carets(&m), vec![1]);
}

#[test]
fn edits_respect_unicode_scalar_boundaries() {
    let mut m = model("héllo 🦀!");
    // The crab is one char; forward-deleting it must not split a codepoint.
    let crab = m.text().chars().position(|c| c == '🦀').unwrap();
    m.set_cursor(crab);
    m.delete_forward();
    assert_eq!(m.text(), "héllo !");
}

// ---- no-ops --------------------------------------------------------------

#[test]
fn backspace_at_the_start_of_the_document_is_a_no_op() {
    let mut m = empty();
    assert_eq!(m.version(), 0);
    assert!(m.backspace().is_none());
    assert_eq!(m.version(), 0, "a no-op must not bump the version");
    assert!(!m.can_undo(), "a no-op must not push an undo step");
}

#[test]
fn delete_forward_at_the_end_of_the_document_is_a_no_op() {
    let mut m = model("ab");
    m.set_cursor(2);
    assert!(m.delete_forward().is_none());
    assert_eq!(m.text(), "ab");
    assert!(!m.can_undo());
}

#[test]
fn inserting_an_empty_string_does_nothing() {
    let mut m = model("ab");
    assert!(m.insert("").is_none());
    assert_eq!(m.version(), 0);
}

// ---- versioning & change summaries ---------------------------------------

#[test]
fn every_text_change_bumps_the_version_by_one() {
    let mut m = empty();
    assert_eq!(m.insert("a").unwrap().version, 1);
    assert_eq!(m.insert("b").unwrap().version, 2);
    assert_eq!(m.backspace().unwrap().version, 3);
    assert_eq!(m.version(), 3);
}

#[test]
fn a_same_line_edit_reports_only_that_line_and_no_line_count_change() {
    let mut m = model("one\ntwo\nthree");
    m.set_cursor(5); // inside line 1 ("two")
    let s = m.insert("X").unwrap();
    assert_eq!(s.kind, EditKind::Insert);
    assert!(!s.line_count_changed);
    assert_eq!(s.changed_lines, 1..2);
}

#[test]
fn inserting_a_newline_reports_a_line_count_change_to_the_bottom() {
    let mut m = model("ab");
    m.set_cursor(1);
    let s = m.insert("\n").unwrap();
    assert!(s.line_count_changed);
    // A structural change repaints from the edit down to the new bottom.
    assert_eq!(s.changed_lines, 0..2);
}

// ---- undo coalescing (no clock) ------------------------------------------

#[test]
fn a_run_of_typing_undoes_as_one_word() {
    // insert() advances the caret itself, so a straight run of single-character
    // inserts — with no caret move between them — is one coalesced typing group.
    let mut m = empty();
    for ch in ["h", "e", "l", "l", "o"] {
        m.insert(ch);
    }
    assert_eq!(m.text(), "hello");
    m.undo();
    assert_eq!(
        m.text(),
        "",
        "the whole typed run collapses into one undo step"
    );
    assert!(!m.can_undo());
}

#[test]
fn a_caret_move_ends_the_typing_run() {
    let mut m = empty();
    m.insert("a");
    m.insert("b");
    m.set_cursor(2); // deliberate caret move
    m.insert("c");
    assert_eq!(m.text(), "abc");
    m.undo();
    assert_eq!(m.text(), "ab", "only the post-move character undoes first");
    m.undo();
    assert_eq!(m.text(), "");
}

#[test]
fn a_newline_is_its_own_undo_step() {
    let mut m = empty();
    m.insert("a");
    m.insert("\n"); // not coalesced with the typing
    m.insert("b");
    assert_eq!(m.text(), "a\nb");
    m.undo();
    assert_eq!(m.text(), "a\n", "the 'b' undoes alone");
    m.undo();
    assert_eq!(m.text(), "a", "the newline undoes alone");
}

#[test]
fn consecutive_deletes_coalesce_but_are_separate_from_typing() {
    let mut m = empty();
    m.insert("a");
    m.insert("b");
    m.insert("c"); // one typing group: "abc"
    m.backspace();
    m.backspace(); // one delete group: removes "c" then "b"
    assert_eq!(m.text(), "a");
    m.undo();
    assert_eq!(m.text(), "abc", "both deletes restore together");
    m.undo();
    assert_eq!(m.text(), "", "the typing group undoes separately");
}

// ---- undo/redo restore selections and resync the highlighter -------------

#[test]
fn undo_and_redo_restore_text_selections_and_bump_the_version() {
    let mut m = model("hello");
    m.set_selections(Selections::single(Selection::new(0, 5)));
    m.insert("x");
    assert_eq!(m.text(), "x");
    let before_versions = m.version();

    let u = m.undo().unwrap();
    assert_eq!(u.kind, EditKind::Undo);
    assert_eq!(m.text(), "hello");
    // Undo re-selects the text that was replaced.
    assert_eq!(*m.selections().primary(), Selection::new(0, 5));
    assert!(m.version() > before_versions);

    let r = m.redo().unwrap();
    assert_eq!(r.kind, EditKind::Redo);
    assert_eq!(m.text(), "x");
    assert_eq!(carets(&m), vec![1]);
}

#[test]
fn a_new_edit_discards_the_redo_branch() {
    let mut m = empty();
    m.insert("a");
    m.undo();
    assert!(m.can_redo());
    m.insert("z");
    assert!(!m.can_redo(), "branching drops the stale redo path");
    assert_eq!(m.text(), "z");
}

#[test]
fn tokens_after_undo_match_the_restored_text() {
    // Regression guard for the resync: the highlighter must reflect the buffer
    // after an undo, not the pre-undo text.
    let mut m = model("abcdef");
    m.set_cursor(6);
    m.insert("\nghijkl"); // now two lines
    assert_eq!(m.len_lines(), 2);
    m.undo();
    assert_eq!(m.len_lines(), 1);

    let mut out = Vec::new();
    m.tokens(0..m.len_lines(), &mut out);
    assert_eq!(out.len(), 1, "one line after undo");
    // PlainHighlighter emits a single token spanning the whole line.
    assert_eq!(out[0].line_len(), 6);
}

// ---- token passthrough ---------------------------------------------------

#[test]
fn tokens_passthrough_covers_the_requested_range() {
    let mut m = model("ab\ncd\nef");
    let mut out = Vec::new();
    m.tokens(0..3, &mut out);
    assert_eq!(out.len(), 3);
    assert!(out.iter().all(|lt| lt.line_len() == 2));
}
