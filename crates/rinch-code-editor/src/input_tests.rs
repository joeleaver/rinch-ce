//! A key is one edit, however many events it arrives as.
//!
//! Since rinch #337 the interceptor hears a key's *release* as well as its
//! press, spelled identically and told apart only by [`KeyEventKind`]. A mapping
//! that ignored the kind applied every key twice — one Enter, two line breaks;
//! one letter, two letters — wherever the backend delivers releases, which is a
//! real desktop keyboard and the debug channel's `key_press` (rinch-ce#8).

use rinch::core::{KeyEventData, KeyEventKind};
use rinch_code_editor_model::{EditorModel, Grid, PlainHighlighter};

use super::apply;
use crate::highlight::dark_theme;
use crate::styles::LINE_HEIGHT;

fn model() -> EditorModel {
    EditorModel::from_text("ab", Box::new(PlainHighlighter::new(dark_theme())))
}

/// Press then release, as a physical keystroke arrives.
fn keystroke(m: &mut EditorModel, key: &str) {
    let grid = Grid::new(1.0, LINE_HEIGHT, 4);
    for kind in [KeyEventKind::Down, KeyEventKind::Up] {
        let ev = KeyEventData::new(key, key).with_kind(kind);
        apply(m, &grid, &ev, 20);
    }
}

#[test]
fn one_enter_is_one_line_break() {
    let mut m = model();
    m.set_cursor(1);
    keystroke(&mut m, "Enter");
    assert_eq!(m.buffer().text(), "a\nb");
}

#[test]
fn one_letter_is_one_letter() {
    let mut m = model();
    m.set_cursor(2);
    keystroke(&mut m, "c");
    assert_eq!(m.buffer().text(), "abc");
}

#[test]
fn one_backspace_deletes_one_character() {
    let mut m = model();
    m.set_cursor(2);
    keystroke(&mut m, "Backspace");
    assert_eq!(m.buffer().text(), "a");
}

#[test]
fn a_release_is_not_claimed() {
    let mut m = model();
    let grid = Grid::new(1.0, LINE_HEIGHT, 4);
    let ev = KeyEventData::new("Enter", "Enter").with_kind(KeyEventKind::Up);
    assert_eq!(apply(&mut m, &grid, &ev, 20), None);
}
