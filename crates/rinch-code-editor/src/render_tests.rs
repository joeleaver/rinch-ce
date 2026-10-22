//! The row helpers must re-run on every edit, not only when the window moves.
//!
//! rinch's memos cut off on equality (rinch #882): a dependent woken only
//! *through* a memo runs only if the memo's value actually changed. The visible
//! line window is such a memo, and typing inside a line leaves it exactly as it
//! was — so a helper that subscribed to the edit only through the window never
//! re-ran, and the keystroke stayed undrawn until a scroll moved the window
//! (rinch-ce#8). Each test here mounts a helper in an effect beside a window
//! memo built the way `ViewState`'s is, makes an edit that leaves the window
//! alone, and checks that the effect saw it.

use std::cell::RefCell;
use std::rc::Rc;

use rinch::core::{Effect, Memo, Signal};
use rinch_code_editor_model::{EditorModel, Grid, RemotePresence, Rgba, Selection, Selections};

use crate::highlight::{dark_theme, make_highlighter};
use crate::presence::remote_carets;
use crate::render::{gutter_rows, selection_rects, text_rows};
use crate::styles::LINE_HEIGHT;

/// A three-line document, its version signal, and a window memo that — like the
/// real one — reads the version but lands on the same range for any edit that
/// does not change the line count.
fn fixture() -> (
    Rc<RefCell<EditorModel>>,
    Signal<u64>,
    Memo<std::ops::Range<usize>>,
) {
    let highlighter = make_highlighter("", dark_theme());
    let model = Rc::new(RefCell::new(EditorModel::from_text(
        "abc\ndef\nghi",
        highlighter,
    )));
    let version = Signal::new(0u64);
    let visible = Memo::new({
        let m = model.clone();
        move || {
            version.get();
            0..m.borrow().len_lines()
        }
    });
    (model, version, visible)
}

fn grid() -> Grid {
    Grid::new(1.0, LINE_HEIGHT, 4)
}

/// Record every value `f` produces from inside an effect.
fn watch<T: 'static>(f: impl Fn() -> T + 'static) -> Rc<RefCell<Vec<T>>> {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let sink = seen.clone();
    // Kept alive by the reactive runtime for the rest of the test thread.
    let _effect = Effect::new(move || sink.borrow_mut().push(f()));
    seen
}

/// The edit the issue reports: one character typed into the middle of a line.
fn type_in_line(model: &Rc<RefCell<EditorModel>>, version: Signal<u64>) {
    {
        let mut m = model.borrow_mut();
        m.set_cursor(5); // "d|ef"
        m.insert("X");
    }
    version.update(|v| *v += 1);
}

#[test]
fn typed_text_reaches_the_rows_without_a_scroll() {
    let (model, version, visible) = fixture();
    let stop_after = Signal::new(usize::MAX);
    let seen = watch({
        let model = model.clone();
        move || {
            let rows = text_rows(version, visible, &model, "#d4d4d4", stop_after);
            rows[1]
                .segs
                .iter()
                .map(|s| s.text.clone())
                .collect::<String>()
        }
    });

    type_in_line(&model, version);

    assert_eq!(seen.borrow().last().map(String::as_str), Some("dXef"));
}

#[test]
fn a_selection_change_reaches_the_rects_without_a_scroll() {
    let (model, version, visible) = fixture();
    let seen = watch({
        let model = model.clone();
        move || selection_rects(version, visible, &model, grid()).len()
    });
    assert_eq!(
        seen.borrow().last(),
        Some(&0),
        "a collapsed caret draws no rect"
    );

    model
        .borrow_mut()
        .set_selections(Selections::single(Selection::new(4, 7)));
    version.update(|v| *v += 1);

    assert_eq!(seen.borrow().last(), Some(&1), "the new selection is drawn");
}

#[test]
fn the_gutter_follows_the_document_without_a_scroll() {
    let (model, version, visible) = fixture();
    let seen = watch({
        let model = model.clone();
        move || gutter_rows(version, visible, &model).len()
    });

    type_in_line(&model, version);

    assert_eq!(
        seen.borrow().len(),
        2,
        "the gutter re-read the edited document"
    );
}

#[test]
fn a_local_edit_moves_a_collaborators_caret_without_a_scroll() {
    let (model, version, visible) = fixture();
    // A peer parked at "de|f". Its offset is the embedder's to keep, and
    // nothing re-sets `peers` after the local edit — the embedder is under no
    // obligation to — so only the version can tell the bar to re-read a
    // document that changed under it.
    let peers = Signal::new(vec![RemotePresence {
        id: 7,
        name: "peer".into(),
        color: Rgba::new(255, 0, 0, 255),
        selection: Selection::cursor(6),
    }]);
    let seen = watch({
        let model = model.clone();
        move || remote_carets(version, visible, peers, &model, grid())[0].vcol
    });
    assert_eq!(seen.borrow().as_slice(), &[2]);

    type_in_line(&model, version);

    assert_eq!(
        seen.borrow().len(),
        2,
        "the caret bar re-read the edited document"
    );
}
