//! The collaborative surface of the model, driven through its public API:
//! recording local changes for publication, and replaying a collaborator's.
//!
//! End-to-end convergence against a real CRDT document lives in the collab
//! crate's `model_sync.rs`; this file pins the model's own behaviour, including
//! the parts a well-behaved document would never think to produce.

use rinch_code_editor_model::{
    EditKind, EditorModel, Highlighter, Selection, Selections, Splice, Theme,
};
use rinch_code_editor_syntax::PlainHighlighter;

fn model(text: &str) -> EditorModel {
    let theme = Theme::builder().build();
    let hl: Box<dyn Highlighter> = Box::new(PlainHighlighter::new(theme));
    EditorModel::from_text(text, hl)
}

/// A model already in a collaborative session.
fn collaborating(text: &str) -> EditorModel {
    let mut m = model(text);
    m.record_local_changes(true);
    m
}

fn carets(m: &EditorModel) -> Vec<usize> {
    m.selections().iter().map(|s| s.head).collect()
}

// ---- recording local changes --------------------------------------------

#[test]
fn recording_is_off_until_asked_for() {
    let mut m = model("hello");
    m.set_cursor(5);
    m.insert("!");
    assert!(
        !m.has_local_changes(),
        "a standalone editor must not accumulate a list nobody drains"
    );
    assert!(m.take_local_changes().is_empty());
}

#[test]
fn every_local_edit_is_recorded_in_order() {
    let mut m = collaborating("");
    m.insert("a");
    m.insert("b");
    m.backspace();
    let changes = m.take_local_changes();
    assert_eq!(changes.len(), 3);

    // Replaying the recorded changes onto a fresh copy of the starting text
    // must reproduce the model exactly — that is the whole contract.
    let mut replay = rinch_code_editor_model::Buffer::from_text("");
    for change in &changes {
        rinch_code_editor_model::Change::apply(&mut replay, change.edits().to_vec());
    }
    assert_eq!(replay.text(), m.text());
    assert_eq!(replay.text(), "a");

    assert!(!m.has_local_changes(), "taking drains the queue");
}

#[test]
fn undo_and_redo_are_recorded_too() {
    // To a shared document reverting is not a rewind, it is another edit. An
    // unpublished undo silently splits the session in two.
    let mut m = collaborating("hello");
    m.set_cursor(5);
    m.insert(" world");
    m.take_local_changes();

    m.undo();
    let undo_changes = m.take_local_changes();
    assert_eq!(undo_changes.len(), 1, "the undo itself must be published");
    assert_eq!(undo_changes[0].edits()[0].range, 5..11);
    assert_eq!(m.text(), "hello");

    m.redo();
    let redo_changes = m.take_local_changes();
    assert_eq!(redo_changes.len(), 1);
    assert_eq!(redo_changes[0].edits()[0].text, " world");
    assert_eq!(m.text(), "hello world");
}

#[test]
fn a_coalesced_typing_run_publishes_every_keystroke() {
    // One undo step, but five changes on the wire: the document must see the
    // text as it was typed, not as it is grouped for undo.
    let mut m = collaborating("");
    for ch in ["h", "e", "l", "l", "o"] {
        m.insert(ch);
    }
    assert_eq!(m.take_local_changes().len(), 5);
    m.undo();
    assert_eq!(m.text(), "", "still one undo step");
    assert_eq!(
        m.take_local_changes().len(),
        5,
        "undone one change per keystroke"
    );
}

#[test]
fn cut_and_paste_are_recorded() {
    let mut m = collaborating("alpha beta");
    m.set_selections(Selections::single(Selection::new(0, 5)));
    m.cut();
    m.paste("gamma", false, None);
    assert_eq!(m.text(), "gamma beta");
    assert_eq!(m.take_local_changes().len(), 2);
}

#[test]
fn turning_recording_off_discards_the_queue() {
    let mut m = collaborating("");
    m.insert("x");
    m.record_local_changes(false);
    assert!(!m.has_local_changes());
    m.insert("y");
    assert!(!m.has_local_changes());
}

// ---- applying remote splices --------------------------------------------

#[test]
fn an_empty_batch_changes_nothing() {
    let mut m = collaborating("hello");
    let version = m.version();
    assert!(m.apply_remote(&[]).is_none());
    assert_eq!(m.version(), version, "not even the version moves");
}

#[test]
fn a_remote_splice_edits_the_text_and_reports_remote() {
    let mut m = collaborating("hello world");
    let summary = m.apply_remote(&[Splice::new(6, 5, "there")]).unwrap();
    assert_eq!(m.text(), "hello there");
    assert_eq!(summary.kind, EditKind::Remote);
    assert_eq!(summary.changed_lines, 0..1);
    assert!(!summary.line_count_changed);
}

#[test]
fn a_batch_of_splices_is_sequential() {
    // Each splice addresses what the one before it produced. Applying them in
    // shared coordinates would put the second one in the wrong place.
    let mut m = collaborating("abcdef");
    m.apply_remote(&[Splice::new(0, 3, "X"), Splice::new(1, 3, "Y")]);
    assert_eq!(m.text(), "XY");
}

#[test]
fn a_remote_edit_is_never_published_back() {
    let mut m = collaborating("hello");
    m.apply_remote(&[Splice::new(0, 0, "oh ")]);
    assert!(
        !m.has_local_changes(),
        "these edits came from the shared document; echoing them back is a loop"
    );
}

#[test]
fn a_remote_edit_leaves_no_undo_step() {
    let mut m = collaborating("hello");
    m.apply_remote(&[Splice::new(0, 0, "oh ")]);
    assert!(!m.can_undo(), "you cannot undo somebody else's typing");
    assert_eq!(m.text(), "oh hello");
}

#[test]
fn a_remote_edit_ends_the_local_typing_run() {
    let mut m = collaborating("");
    m.insert("a");
    m.insert("b");
    m.take_local_changes();
    m.apply_remote(&[Splice::new(0, 0, "-")]);
    m.insert("c");
    // The run was broken, so "c" is its own step rather than a continuation of
    // one recorded against text that has since moved.
    m.undo();
    assert_eq!(m.text(), "-ab");
}

#[test]
fn remote_line_changes_are_summarized_for_the_view() {
    let mut m = collaborating("one\ntwo\nthree");
    let summary = m.apply_remote(&[Splice::new(4, 0, "half\n")]).unwrap();
    assert_eq!(m.text(), "one\nhalf\ntwo\nthree");
    assert!(summary.line_count_changed);
    assert_eq!(
        summary.changed_lines,
        1..4,
        "from the edit down: every line below shifted"
    );

    // "one\nhalf\ntwo\nthree": line 2 ("two") starts at offset 9.
    let summary = m.apply_remote(&[Splice::new(9, 3, "TW")]).unwrap();
    assert_eq!(m.text(), "one\nhalf\nTW\nthree");
    assert!(!summary.line_count_changed);
    assert_eq!(summary.changed_lines, 2..3, "one line rewritten in place");
}

// ---- selections ride along ----------------------------------------------

#[test]
fn a_remote_edit_before_the_caret_carries_it_along() {
    let mut m = collaborating("hello");
    m.set_cursor(5);
    m.apply_remote(&[Splice::new(0, 0, "oh ")]);
    assert_eq!(carets(&m), vec![8]);
}

#[test]
fn a_remote_insertion_at_the_caret_leaves_it_put() {
    // The bias that matters: a collaborator typing at your position must not
    // drag your cursor along behind their text.
    let mut m = collaborating("hello");
    m.set_cursor(5);
    m.apply_remote(&[Splice::new(5, 0, " world")]);
    assert_eq!(carets(&m), vec![5]);
    assert_eq!(m.text(), "hello world");
}

#[test]
fn a_caret_inside_remotely_replaced_text_collapses_to_the_start() {
    let mut m = collaborating("hello world");
    m.set_cursor(8);
    m.apply_remote(&[Splice::new(6, 5, "everybody")]);
    assert_eq!(carets(&m), vec![6]);
}

#[test]
fn a_remote_edit_after_the_caret_leaves_it_alone() {
    let mut m = collaborating("hello world");
    m.set_cursor(3);
    m.apply_remote(&[Splice::new(6, 5, "everybody")]);
    assert_eq!(carets(&m), vec![3]);
}

#[test]
fn multiple_carets_all_ride_the_same_remote_edit() {
    let mut m = collaborating("aa\nbb\ncc");
    m.set_selections(Selections::new(
        vec![
            Selection::cursor(0),
            Selection::cursor(3),
            Selection::cursor(6),
        ],
        0,
    ));
    m.apply_remote(&[Splice::new(0, 0, ">>")]);
    // The first caret sits exactly at the insertion point, so it holds its
    // place and the remote text lands after it; the other two ride along.
    assert_eq!(carets(&m), vec![0, 5, 8]);
}

// ---- the resync path -----------------------------------------------------

#[test]
fn a_whole_buffer_replacement_keeps_the_caret_in_place() {
    // The collab layer's resync: same document, freshly stated. Collapsing
    // every caret to offset 0 (what the general rule would do, since every
    // offset is inside the replaced span) would be a lie about what happened.
    let mut m = collaborating("hello world");
    m.set_cursor(8);
    m.apply_remote(&[Splice::new(0, 11, "hello there world")]);
    assert_eq!(m.text(), "hello there world");
    assert_eq!(carets(&m), vec![8]);
}

#[test]
fn a_resync_that_restates_the_current_text_does_nothing_at_all() {
    // The collaborative layer resynchronizes on any patch it cannot project
    // onto plain text, and a peer can provoke that at will — a block marker on
    // every paragraph, say. The text it hands back is usually identical to what
    // we already hold, so treating it as an edit would repaint the document,
    // bump the version and (until the history is rebased rather than dropped)
    // cost the user their undo stack, on every keystroke that peer types.
    let mut m = collaborating("hello world");
    m.set_cursor(5);
    m.insert("!");
    m.take_local_changes();
    let version = m.version();
    assert!(m.can_undo());

    assert!(
        m.apply_remote(&[Splice::new(0, 12, "hello! world")])
            .is_none(),
        "a restatement of the current text is not an edit"
    );
    assert_eq!(m.version(), version, "no version bump, so no repaint");
    assert!(m.can_undo(), "and the undo history survives");
    assert_eq!(carets(&m), vec![6]);
}

#[test]
fn a_resync_that_really_changes_the_text_still_applies() {
    let mut m = collaborating("hello world");
    m.set_cursor(5);
    let summary = m
        .apply_remote(&[Splice::new(0, 11, "hello there")])
        .unwrap();
    assert_eq!(m.text(), "hello there");
    assert_eq!(summary.kind, EditKind::Remote);
}

#[test]
fn a_batch_of_only_restatements_reports_nothing() {
    let mut m = collaborating("abc");
    let version = m.version();
    assert!(
        m.apply_remote(&[Splice::new(0, 3, "abc"), Splice::new(0, 3, "abc")])
            .is_none()
    );
    assert_eq!(m.version(), version);
}

#[test]
fn a_malformed_remote_splice_is_clamped_rather_than_trusted() {
    // Splices arrive from the network. `pos + del` must not wrap, the buffer
    // must not be addressed past its end, and no selection may escape it.
    let mut m = collaborating("abc");
    m.set_cursor(3);
    m.apply_remote(&[Splice::new(2, usize::MAX, "Z")]);
    assert_eq!(m.text(), "abZ");
    assert!(
        carets(&m).iter().all(|c| *c <= m.len_chars()),
        "a caret outside the buffer would poison the next local edit"
    );

    m.apply_remote(&[Splice::new(usize::MAX, 7, "!")]);
    assert_eq!(m.text(), "abZ!");
    assert!(carets(&m).iter().all(|c| *c <= m.len_chars()));
}

#[test]
#[should_panic(expected = "not recording local changes")]
fn applying_remote_splices_without_recording_is_refused() {
    // Receiving a collaborator's edits while not publishing your own is never
    // coherent: every local edit is silently failing to reach the shared
    // document, and the unpublished-changes assertion can never fire because
    // the queue can never fill.
    let mut m = model("hello");
    m.apply_remote(&[Splice::new(0, 0, "oh ")]);
}

#[test]
fn a_resync_onto_a_shorter_text_clamps_the_caret() {
    let mut m = collaborating("hello world");
    m.set_cursor(11);
    m.apply_remote(&[Splice::new(0, 11, "hi")]);
    assert_eq!(m.text(), "hi");
    assert_eq!(carets(&m), vec![2], "clamped, never out of range");
}

#[test]
fn the_joiner_bootstrap_splice_is_an_ordinary_insertion() {
    // An empty document receiving its whole content: `del` is 0, so this is
    // not the resync path at all, and the caret at 0 simply stays there.
    let mut m = collaborating("");
    let summary = m.apply_remote(&[Splice::new(0, 0, "line one\nline two")]);
    assert_eq!(m.text(), "line one\nline two");
    assert_eq!(carets(&m), vec![0]);
    assert!(summary.unwrap().line_count_changed);
}

// ---- ordering ------------------------------------------------------------

#[test]
#[should_panic(expected = "unpublished local changes")]
fn applying_remote_splices_over_unpublished_local_edits_is_refused() {
    // The splices were computed against a document that has not seen the local
    // insert, so every offset in them is stale. Silent divergence is the one
    // outcome a collaborative editor must never have.
    let mut m = collaborating("hello");
    m.set_cursor(0);
    m.insert("X");
    m.apply_remote(&[Splice::new(5, 0, "!")]);
}

#[test]
fn publishing_first_makes_the_batch_applicable() {
    let mut m = collaborating("hello");
    m.set_cursor(0);
    m.insert("X");
    m.take_local_changes();
    m.apply_remote(&[Splice::new(6, 0, "!")]);
    assert_eq!(m.text(), "Xhello!");
}
