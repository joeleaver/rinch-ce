//! Keyboard input: turning key events into model operations.
//!
//! rinch routes keys through a single global interceptor rather than per-element
//! `onkeydown`, so [`install`] registers one closure that owns a handle to the
//! model. It is gated on a focus flag the component sets when the editor is
//! clicked, so keystrokes only reach the document while the editor has focus and
//! otherwise fall through to the runtime untouched.
//!
//! The mapping in [`apply`] is intentionally small and Monaco-shaped: arrows and
//! their Ctrl/Shift variants move or extend, the edit keys insert and delete, and
//! a bare printable character types. Everything the editor does not claim returns
//! `false` so the runtime still sees it.

use std::cell::RefCell;
use std::rc::Rc;

use rinch::core::{KeyEventData, Signal, set_keyboard_interceptor};
use rinch_code_editor_model::{EditorModel, Grid, Motion, Selection, Selections};

/// How many lines Page Up/Down move until the view can report its own height. A
/// fixed step is a stand-in; Increment 4d makes it the real visible-line count.
const PAGE_LINES: usize = 20;

/// Install the global keyboard interceptor. It drives `model`, bumps `version`
/// so the reactive render repaints, and does nothing unless `focused` is set.
pub(crate) fn install(
    model: Rc<RefCell<EditorModel>>,
    grid: Grid,
    version: Signal<u64>,
    focused: Signal<bool>,
) {
    set_keyboard_interceptor(move |ev| {
        if !focused.get() {
            return false;
        }
        // Confine the model borrow to this block. Bumping `version` below runs
        // the reactive render effects synchronously, and those read the model —
        // so its borrow must be released first, or they panic on a double borrow.
        let outcome = {
            let mut m = model.borrow_mut();
            apply(&mut m, &grid, ev)
        };
        match outcome {
            None => false,
            Some(changed) => {
                if changed {
                    version.update(|v| *v = v.wrapping_add(1));
                }
                true
            }
        }
    });
}

/// Map one key event to a model operation. `None` means the editor does not
/// handle this key (the runtime keeps it); `Some(changed)` means it is consumed,
/// with `changed` telling whether the document or selection actually moved — an
/// edit that is a no-op (backspace at the very start) is consumed but changes
/// nothing, so it does not trigger a repaint.
fn apply(m: &mut EditorModel, grid: &Grid, ev: &KeyEventData) -> Option<bool> {
    // Ctrl combos first. On desktop `meta` is always false — Cmd arrives as ctrl
    // — so ctrl is the one modifier that carries shortcuts.
    if ev.ctrl && !ev.alt {
        match ev.key.as_str() {
            "z" | "Z" if ev.shift => return Some(m.redo().is_some()),
            "z" | "Z" => return Some(m.undo().is_some()),
            "y" | "Y" => return Some(m.redo().is_some()),
            "a" | "A" => {
                select_all(m);
                return Some(true);
            }
            _ => {}
        }
    }

    let extend = ev.shift;
    match ev.key.as_str() {
        "ArrowLeft" => moved(
            m,
            grid,
            if ev.ctrl {
                Motion::WordLeft
            } else {
                Motion::Left
            },
            extend,
        ),
        "ArrowRight" => moved(
            m,
            grid,
            if ev.ctrl {
                Motion::WordRight
            } else {
                Motion::Right
            },
            extend,
        ),
        "ArrowUp" => moved(m, grid, Motion::Up(1), extend),
        "ArrowDown" => moved(m, grid, Motion::Down(1), extend),
        "PageUp" => moved(m, grid, Motion::Up(PAGE_LINES), extend),
        "PageDown" => moved(m, grid, Motion::Down(PAGE_LINES), extend),
        "Home" => moved(
            m,
            grid,
            if ev.ctrl {
                Motion::DocStart
            } else {
                Motion::LineStart
            },
            extend,
        ),
        "End" => moved(
            m,
            grid,
            if ev.ctrl {
                Motion::DocEnd
            } else {
                Motion::LineEnd
            },
            extend,
        ),
        "Backspace" => Some(m.backspace().is_some()),
        "Delete" => Some(m.delete_forward().is_some()),
        "Enter" => Some(m.insert("\n").is_some()),
        "Tab" => Some(m.insert("\t").is_some()),
        key => {
            if is_printable(key) && !ev.ctrl && !ev.alt && !ev.meta {
                Some(m.insert(key).is_some())
            } else {
                None
            }
        }
    }
}

/// Apply a cursor motion and report it consumed. A motion always redraws the
/// caret, even when it lands where it started (arrow at a document edge), which
/// matches every editor.
fn moved(m: &mut EditorModel, grid: &Grid, motion: Motion, extend: bool) -> Option<bool> {
    m.move_cursors(grid, motion, extend);
    Some(true)
}

/// Collapse to one selection spanning the whole document, caret at the end.
fn select_all(m: &mut EditorModel) {
    let end = m.len_chars();
    m.set_selections(Selections::single(Selection::new(0, end)));
}

/// A key value that is one printable character: a single Unicode scalar that is
/// not a control character. Named keys ("Enter", "Shift", "ArrowLeft") are longer
/// than one char, so they are never mistaken for text.
fn is_printable(key: &str) -> bool {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => !c.is_control(),
        _ => false,
    }
}
