//! Behavioural tests for the clipboard's copy payloads and cut deletes. Each
//! rule is Monaco's, ported from the vscode source (see the clipboard module
//! docs); the tests double as the spec for the view layer's Ctrl+X/C. Paste
//! lives in `clipboard_paste.rs` — one operation with three distribution
//! modes earns its own file.

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

// ---- copy: the payload ----------------------------------------------------

#[test]
fn copy_of_a_selection_is_exactly_its_text() {
    let mut m = model("hello world");
    m.set_selections(Selections::single(Selection::new(0, 5)));
    let p = m.clipboard_payload(false);
    assert_eq!(p.text, "hello");
    assert!(!p.is_from_empty_selection);
    assert_eq!(p.multicursor_text, None);
}

#[test]
fn copy_of_a_reversed_selection_is_the_same_text() {
    let mut m = model("hello world");
    m.set_selections(Selections::single(Selection::new(5, 0)));
    assert_eq!(m.clipboard_payload(false).text, "hello");
}

#[test]
fn copy_with_an_empty_cursor_takes_the_whole_line_and_flags_it() {
    let mut m = model("one\ntwo\nthree");
    m.set_cursor(5); // inside "two"
    let p = m.clipboard_payload(false);
    assert_eq!(p.text, "two\n");
    assert!(p.is_from_empty_selection, "the whole-line copy is flagged");
    assert_eq!(p.multicursor_text, None);
}

#[test]
fn copy_on_an_empty_line_takes_just_the_newline() {
    let mut m = model("aa\n\nbb");
    m.set_cursor(3); // the empty middle line
    let p = m.clipboard_payload(false);
    assert_eq!(p.text, "\n");
    assert!(p.is_from_empty_selection);
}

#[test]
fn copy_with_two_empty_cursors_takes_both_lines_unflagged() {
    let mut m = model("aa\nbb");
    cursors(&mut m, &[0, 3]);
    let p = m.clipboard_payload(false);
    // Each whole-line piece carries its newline, and pieces join with one more
    // — faithful to Monaco's getPlainTextToCopy + join.
    assert_eq!(p.text, "aa\n\nbb\n");
    assert!(
        !p.is_from_empty_selection,
        "only a single empty cursor is a whole-line copy"
    );
    assert_eq!(
        p.multicursor_text,
        Some(vec!["aa\n".to_string(), "bb\n".to_string()])
    );
}

#[test]
fn copy_dedupes_empty_cursors_on_the_same_line() {
    let mut m = model("hello");
    cursors(&mut m, &[1, 3]);
    let p = m.clipboard_payload(false);
    assert_eq!(p.text, "hello\n", "the line is copied once, not twice");
    assert_eq!(
        p.multicursor_text, None,
        "one piece, so no multicursor list"
    );
}

#[test]
fn copy_mixes_selection_text_with_whole_lines() {
    let mut m = model("aa\nbb\ncc");
    m.set_selections(Selections::new(
        vec![Selection::new(0, 2), Selection::cursor(4)],
        0,
    ));
    let p = m.clipboard_payload(false);
    // "aa" exactly, then the whole of "bb" with its newline.
    assert_eq!(p.text, "aa\nbb\n");
    assert_eq!(
        p.multicursor_text,
        Some(vec!["aa".to_string(), "bb\n".to_string()])
    );
}

#[test]
fn copy_skips_an_empty_cursor_on_a_line_a_selection_started_on() {
    let mut m = model("abcdef");
    m.set_selections(Selections::new(
        vec![Selection::new(1, 3), Selection::cursor(5)],
        0,
    ));
    let p = m.clipboard_payload(false);
    // The empty cursor's line is the one the selection started on — deduped.
    assert_eq!(p.text, "bc");
    assert_eq!(p.multicursor_text, None);
}

#[test]
fn copy_of_two_selections_joins_them_and_keeps_the_pieces() {
    let mut m = model("aa\nbb");
    m.set_selections(Selections::new(
        vec![Selection::new(0, 2), Selection::new(3, 5)],
        0,
    ));
    let p = m.clipboard_payload(false);
    assert_eq!(p.text, "aa\nbb");
    assert!(!p.is_from_empty_selection);
    assert_eq!(
        p.multicursor_text,
        Some(vec!["aa".to_string(), "bb".to_string()])
    );
}

#[test]
fn copy_of_a_multiline_selection_is_one_piece() {
    let mut m = model("aa\nbb\ncc");
    m.set_selections(Selections::single(Selection::new(1, 5)));
    let p = m.clipboard_payload(false);
    assert_eq!(
        p.text, "a\nbb",
        "exactly the selected text, newline included"
    );
    assert!(!p.is_from_empty_selection);
    assert_eq!(p.multicursor_text, None);
}

#[test]
fn copy_does_not_dedupe_against_a_selections_end_line() {
    // The dedupe keys on the previous selection's START line (Monaco's
    // prevModelLineNumber) — a selection merely ending on the cursor's line
    // must not swallow the cursor's whole-line piece.
    let mut m = model("aa\nbb\ncc");
    m.set_selections(Selections::new(
        vec![Selection::new(0, 4), Selection::cursor(5)],
        0,
    ));
    let p = m.clipboard_payload(false);
    assert_eq!(p.text, "aa\nb\nbb\n");
    assert_eq!(
        p.multicursor_text,
        Some(vec!["aa\nb".to_string(), "bb\n".to_string()])
    );
}

#[test]
fn copy_with_force_crlf_uses_windows_line_endings_throughout() {
    // Monaco passes isWindows as forceCRLF: pieces, whole-line trailers, and
    // the join all become \r\n on the Windows clipboard.
    let mut m = model("aa\nbb\ncc");
    m.set_selections(Selections::single(Selection::new(0, 5)));
    assert_eq!(m.clipboard_payload(true).text, "aa\r\nbb");

    m.set_cursor(4); // whole-line copy of "bb"
    let p = m.clipboard_payload(true);
    assert_eq!(p.text, "bb\r\n");
    assert!(p.is_from_empty_selection);

    m.set_selections(Selections::new(
        vec![Selection::new(0, 2), Selection::new(3, 5)],
        0,
    ));
    let p = m.clipboard_payload(true);
    assert_eq!(p.text, "aa\r\nbb", "pieces join with \\r\\n");
    assert_eq!(
        p.multicursor_text,
        Some(vec!["aa".to_string(), "bb".to_string()])
    );
}

// ---- cut ------------------------------------------------------------------

#[test]
fn cut_of_a_selection_deletes_it_and_lands_at_its_start() {
    let mut m = model("hello world");
    m.set_selections(Selections::single(Selection::new(5, 11)));
    let s = m.cut().unwrap();
    assert_eq!(s.kind, EditKind::Delete);
    assert_eq!(m.text(), "hello");
    assert_eq!(carets(&m), vec![5]);
}

#[test]
fn cut_of_an_empty_cursor_removes_the_whole_middle_line() {
    let mut m = model("aa\nbb\ncc");
    m.set_cursor(4); // inside "bb"
    m.cut().unwrap();
    assert_eq!(m.text(), "aa\ncc", "content and terminator both go");
    assert_eq!(carets(&m), vec![3], "column 0 of the line that moved up");
}

#[test]
fn cut_of_the_first_line_removes_it_and_its_terminator() {
    let mut m = model("aa\nbb");
    m.set_cursor(1);
    m.cut().unwrap();
    assert_eq!(m.text(), "bb");
    assert_eq!(carets(&m), vec![0]);
}

#[test]
fn cut_of_the_last_line_takes_the_preceding_newline_too() {
    let mut m = model("aa\nbb");
    m.set_cursor(4); // inside "bb", the last line
    m.cut().unwrap();
    assert_eq!(m.text(), "aa");
    assert_eq!(carets(&m), vec![2], "end of the new last line");
}

#[test]
fn cut_on_a_single_line_document_clears_the_line() {
    let mut m = model("hello");
    m.set_cursor(3);
    m.cut().unwrap();
    assert_eq!(m.text(), "");
    assert_eq!(carets(&m), vec![0]);
}

#[test]
fn cut_in_an_empty_document_is_a_noop() {
    let mut m = model("");
    assert!(m.cut().is_none());
    assert!(!m.can_undo(), "a no-op cut records nothing");
    // The copy half still has a payload — the empty line's newline.
    assert_eq!(m.clipboard_payload(false).text, "\n");
}

#[test]
fn cut_with_two_cursors_on_one_line_deletes_it_once() {
    let mut m = model("aa\nbb\ncc");
    cursors(&mut m, &[3, 5]); // both inside "bb"
    m.cut().unwrap();
    assert_eq!(m.text(), "aa\ncc");
    assert_eq!(
        carets(&m),
        vec![3],
        "the two carets merge where the line was"
    );
}

#[test]
fn cut_with_cursors_on_the_last_two_lines_removes_both() {
    let mut m = model("aa\nbb\ncc");
    cursors(&mut m, &[4, 7]); // "bb" (middle) and "cc" (last)
    m.cut().unwrap();
    // The middle-line cut already ends on the last line, so the last line's
    // cut clears its content instead of taking the preceding newline again.
    assert_eq!(m.text(), "aa\n");
    assert_eq!(carets(&m), vec![3]);
}

#[test]
fn cut_reroute_on_a_trailing_empty_line_still_merges_the_caret() {
    // The middle-line cut ends on the trailing empty line, so the second
    // cursor's rerouted range is empty: it contributes no edit, but its caret
    // must still ride the earlier deletion left and merge.
    let mut m = model("aa\nbb\n");
    cursors(&mut m, &[4, 6]);
    m.cut().unwrap();
    assert_eq!(m.text(), "aa\n");
    assert_eq!(carets(&m), vec![3]);
}

#[test]
fn cut_with_a_selection_and_a_cursor_on_its_line_drops_the_line_cut() {
    let mut m = model("hello world");
    m.set_selections(Selections::new(
        vec![Selection::new(0, 5), Selection::cursor(8)],
        0,
    ));
    m.cut().unwrap();
    // The empty cursor's whole-line delete overlaps the selection's — the
    // later cursor loses and only the selection is cut.
    assert_eq!(m.text(), " world");
    assert_eq!(carets(&m), vec![0]);
}

#[test]
fn cut_undo_restores_text_and_selections_in_one_step() {
    let mut m = model("aa\nbb\ncc");
    m.set_cursor(4);
    m.insert("X"); // some typing first
    m.cut().unwrap();
    assert_eq!(m.text(), "aa\ncc");
    m.undo();
    assert_eq!(m.text(), "aa\nbXb\ncc", "the cut undoes alone");
    assert_eq!(carets(&m), vec![5], "the pre-cut caret comes back");
    m.undo();
    assert_eq!(m.text(), "aa\nbb\ncc", "the typing undoes separately");
}

#[test]
fn cut_redo_reapplies_text_and_carets() {
    let mut m = model("aa\nbb\ncc");
    m.set_cursor(4);
    m.cut().unwrap();
    m.undo();
    let s = m.redo().unwrap();
    assert_eq!(s.kind, EditKind::Redo);
    assert_eq!(m.text(), "aa\ncc");
    assert_eq!(carets(&m), vec![3]);
}

#[test]
fn cut_summary_reports_changed_lines() {
    let mut m = model("aa\nbb\ncc");
    m.set_cursor(4);
    let s = m.cut().unwrap();
    assert_eq!(s.version, 1);
    assert_eq!(m.version(), 1);
    assert!(s.line_count_changed);
    assert_eq!(s.changed_lines, 1..2);
}
