//! Behavioural tests for paste — Monaco's `PasteOperation.getEdits`, ported
//! in the model's clipboard module. Copy and cut live in `clipboard.rs`;
//! paste's three distribution modes (matching multicursor pieces, the
//! 'spread' heuristic, the whole-line paste-above) earn their own file.

use rinch_code_editor_model::{EditKind, EditorModel, Highlighter, Selection, Selections, Theme};
use rinch_code_editor_syntax::PlainHighlighter;

fn model(text: &str) -> EditorModel {
    let theme = Theme::builder().build();
    let hl: Box<dyn Highlighter> = Box::new(PlainHighlighter::new(theme));
    EditorModel::from_text(text, hl)
}

fn carets(m: &EditorModel) -> Vec<usize> {
    m.selections().iter().map(|s| s.head).collect()
}

fn cursors(m: &mut EditorModel, offsets: &[usize]) {
    m.set_selections(Selections::new(
        offsets.iter().map(|&o| Selection::cursor(o)).collect(),
        0,
    ));
}

// ---- plain paste ----------------------------------------------------------

#[test]
fn paste_inserts_at_the_caret() {
    let mut m = model("ab");
    m.set_cursor(1);
    let s = m.paste("XY", false, None).unwrap();
    assert_eq!(s.kind, EditKind::Insert);
    assert_eq!(m.text(), "aXYb");
    assert_eq!(carets(&m), vec![3]);
}

#[test]
fn paste_replaces_a_selection() {
    let mut m = model("hello world");
    m.set_selections(Selections::single(Selection::new(0, 5)));
    m.paste("bye", false, None).unwrap();
    assert_eq!(m.text(), "bye world");
    assert_eq!(carets(&m), vec![3]);
}

#[test]
fn paste_normalizes_crlf_to_the_model_eol() {
    let mut m = model("");
    m.paste("x\r\ny\rz", false, None).unwrap();
    assert_eq!(m.text(), "x\ny\nz");
    assert_eq!(carets(&m), vec![5]);
}

#[test]
fn paste_of_nothing_is_a_noop() {
    let mut m = model("ab");
    assert!(m.paste("", false, None).is_none());
    assert!(!m.can_undo());
    assert_eq!(m.version(), 0, "a no-op must not bump the version");
}

// ---- the whole-line copy: paste above -------------------------------------

#[test]
fn paste_of_a_whole_line_copy_lands_above_and_preserves_the_caret() {
    let mut m = model("one\ntwo");
    m.set_cursor(5); // "two", column 1
    m.paste("XX\n", true, None).unwrap();
    assert_eq!(m.text(), "one\nXX\ntwo", "the line goes in above");
    // The caret stays on its character — same column, one line down.
    assert_eq!(carets(&m), vec![8]);
}

#[test]
fn paste_of_a_whole_line_copy_at_column_zero() {
    let mut m = model("one\ntwo");
    m.set_cursor(4);
    m.paste("XX\n", true, None).unwrap();
    assert_eq!(m.text(), "one\nXX\ntwo");
    assert_eq!(carets(&m), vec![7], "still at column 0 of its line");
}

#[test]
fn paste_of_a_whole_line_copy_on_the_final_empty_line() {
    // The caret sits on the trailing empty line: the insert lands at the very
    // end of the document and the preserved caret rides it to the new end.
    let mut m = model("one\n");
    m.set_cursor(4);
    m.paste("XX\n", true, None).unwrap();
    assert_eq!(m.text(), "one\nXX\n");
    assert_eq!(carets(&m), vec![7]);
}

#[test]
fn paste_of_a_whole_line_copy_over_a_selection_is_a_plain_replace() {
    let mut m = model("abcdef");
    m.set_selections(Selections::single(Selection::new(1, 3)));
    m.paste("XX\n", true, None).unwrap();
    assert_eq!(m.text(), "aXX\ndef");
    assert_eq!(carets(&m), vec![4]);
}

#[test]
fn paste_on_new_line_needs_exactly_one_trailing_newline() {
    // Two lines: plain insert at the caret.
    let mut m = model("one\ntwo");
    m.set_cursor(5);
    m.paste("a\nb\n", true, None).unwrap();
    assert_eq!(m.text(), "one\nta\nb\nwo");

    // No trailing newline: plain insert too.
    let mut m = model("one\ntwo");
    m.set_cursor(5);
    m.paste("XX", true, None).unwrap();
    assert_eq!(m.text(), "one\ntXXwo");
}

#[test]
fn paste_on_new_line_latches_off_at_the_first_nonempty_selection() {
    // A selection sorts first and demotes the whole paste to plain — the empty
    // cursor after it gets the text at its caret, not above its line.
    let mut m = model("abcd\nefgh");
    m.set_selections(Selections::new(
        vec![Selection::new(0, 2), Selection::cursor(7)],
        0,
    ));
    m.paste("zz\n", true, None).unwrap();
    assert_eq!(m.text(), "zz\ncd\nefzz\ngh");
    assert_eq!(carets(&m), vec![3, 11]);
}

#[test]
fn paste_of_a_whole_line_copy_onto_two_cursors_on_one_line() {
    let mut m = model("cc\ndd");
    cursors(&mut m, &[3, 4]); // both on "dd"
    m.paste("xx\n", true, None).unwrap();
    // Each cursor inserts the line above; both carets keep their character.
    assert_eq!(m.text(), "cc\nxx\nxx\ndd");
    assert_eq!(carets(&m), vec![9, 10]);
}

#[test]
fn paste_of_a_whole_line_copy_onto_cursors_on_different_lines() {
    let mut m = model("aa\nbb");
    cursors(&mut m, &[1, 4]);
    m.paste("xx\n", true, None).unwrap();
    assert_eq!(m.text(), "xx\naa\nxx\nbb");
    assert_eq!(carets(&m), vec![4, 10]);
}

#[test]
fn paste_on_new_line_with_multibyte_text() {
    // The exactly-one-line gate works on byte indices, the caret math on
    // chars; multi-byte text pins that neither mixes units.
    let mut m = model("αβ\nγδ");
    m.set_cursor(4);
    m.paste("🦀é\n", true, None).unwrap();
    assert_eq!(m.text(), "αβ\n🦀é\nγδ");
    assert_eq!(carets(&m), vec![7]);
}

// ---- distribution: multicursor pieces and 'spread' ------------------------

#[test]
fn paste_distributes_matching_multicursor_pieces() {
    let mut m = model("a b");
    cursors(&mut m, &[0, 2]);
    let pieces = vec!["X".to_string(), "Y".to_string()];
    m.paste("X\nY", false, Some(&pieces)).unwrap();
    assert_eq!(m.text(), "Xa Yb");
    assert_eq!(carets(&m), vec![1, 4]);
}

#[test]
fn paste_multicursor_pieces_win_over_paste_on_new_line() {
    // Monaco checks the piece-count match before the pasteOnNewLine bailout.
    let mut m = model("a b");
    cursors(&mut m, &[0, 2]);
    let pieces = vec!["X".to_string(), "Y".to_string()];
    m.paste("X\nY", true, Some(&pieces)).unwrap();
    assert_eq!(m.text(), "Xa Yb");
}

#[test]
fn paste_distributes_multiline_pieces() {
    let mut m = model("AB");
    cursors(&mut m, &[1, 2]);
    let pieces = vec!["x\ny".to_string(), "p\nq".to_string()];
    m.paste("x\ny\np\nq", false, Some(&pieces)).unwrap();
    assert_eq!(m.text(), "Ax\nyBp\nq");
    assert_eq!(carets(&m), vec![4, 8]);
}

#[test]
fn paste_spreads_lines_across_matching_cursor_count() {
    let mut m = model("1\n2\n3");
    cursors(&mut m, &[0, 2, 4]);
    m.paste("a\nb\nc", false, None).unwrap();
    assert_eq!(m.text(), "a1\nb2\nc3");
    assert_eq!(carets(&m), vec![1, 4, 7]);
}

#[test]
fn paste_spread_forgives_one_trailing_newline() {
    let mut m = model("1\n2");
    cursors(&mut m, &[0, 2]);
    m.paste("a\nb\n", false, None).unwrap();
    assert_eq!(m.text(), "a1\nb2");
}

#[test]
fn paste_spread_handles_crlf_text() {
    let mut m = model("x\ny");
    cursors(&mut m, &[0, 2]);
    m.paste("a\r\nb\r\n", false, None).unwrap();
    assert_eq!(m.text(), "ax\nby");
}

#[test]
fn paste_spread_with_multibyte_pieces() {
    let mut m = model("a\nb");
    cursors(&mut m, &[0, 2]);
    m.paste("🦀é\nx", false, None).unwrap();
    assert_eq!(m.text(), "🦀éa\nxb");
    assert_eq!(carets(&m), vec![2, 5], "deltas are chars, not bytes");
}

#[test]
fn paste_spread_with_an_empty_line_skips_that_cursors_edit() {
    let mut m = model("1\n2\n3");
    cursors(&mut m, &[0, 2, 4]);
    m.paste("a\n\nc", false, None).unwrap();
    assert_eq!(m.text(), "a1\n2\nc3");
    assert_eq!(
        carets(&m),
        vec![1, 3, 6],
        "the empty piece's caret still rides the earlier insert"
    );
    m.undo();
    assert_eq!(m.text(), "1\n2\n3", "one honest undo step");
}

#[test]
fn paste_on_new_line_prevents_spreading() {
    // Monaco's order of checks: the pasteOnNewLine bailout comes BEFORE the
    // spread heuristic, so a whole-line-copy flag stops line-per-cursor
    // distribution even when the counts match.
    let mut m = model("1\n2");
    cursors(&mut m, &[0, 2]);
    m.paste("a\nb\n", true, None).unwrap();
    // Not distributed; and two lines is not exactly-one-line, so the
    // whole-line insert demotes to a plain insert at each caret.
    assert_eq!(m.text(), "a\nb\n1\na\nb\n2");
    assert_eq!(carets(&m), vec![4, 10]);
}

#[test]
fn paste_with_mismatched_counts_inserts_the_whole_text_everywhere() {
    let mut m = model("1\n2");
    cursors(&mut m, &[0, 2]);
    m.paste("a\nb\nc", false, None).unwrap();
    assert_eq!(m.text(), "a\nb\nc1\na\nb\nc2");
    // A stale two-piece list on three cursors falls through the same way.
    let mut m = model("1\n2\n3");
    cursors(&mut m, &[0, 2, 4]);
    let stale = vec!["X".to_string(), "Y".to_string()];
    m.paste("X", false, Some(&stale)).unwrap();
    assert_eq!(m.text(), "X1\nX2\nX3");
}

// ---- history and repaint summaries ----------------------------------------

#[test]
fn paste_undo_restores_text_and_selections_in_one_step() {
    let mut m = model("hello");
    m.set_selections(Selections::single(Selection::new(0, 5)));
    m.paste("bye\nbye", false, None).unwrap();
    assert_eq!(m.text(), "bye\nbye");
    m.undo();
    assert_eq!(m.text(), "hello");
    assert_eq!(*m.selections().primary(), Selection::new(0, 5));
    m.redo();
    assert_eq!(m.text(), "bye\nbye");
    assert_eq!(carets(&m), vec![7]);
}

#[test]
fn paste_above_undoes_in_one_step_and_restores_the_caret() {
    let mut m = model("one\ntwo");
    m.set_cursor(5);
    m.paste("XX\n", true, None).unwrap();
    m.undo();
    assert_eq!(m.text(), "one\ntwo");
    assert_eq!(carets(&m), vec![5]);
}

#[test]
fn paste_summary_without_a_newline_stays_on_one_line() {
    let mut m = model("ab");
    m.set_cursor(1);
    let s = m.paste("XY", false, None).unwrap();
    assert_eq!(s.version, 1);
    assert!(!s.line_count_changed);
    assert_eq!(s.changed_lines, 0..1);
}

#[test]
fn paste_above_summary_reports_the_line_growth() {
    let mut m = model("one\ntwo");
    m.set_cursor(5);
    let s = m.paste("XX\n", true, None).unwrap();
    assert!(s.line_count_changed);
    assert_eq!(s.changed_lines, 1..3);
}
