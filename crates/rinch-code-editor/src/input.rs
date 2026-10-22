//! Keyboard input: turning key events into model operations.
//!
//! rinch routes keys through a single *document-wide* interceptor rather than
//! per-element `onkeydown`, so the interceptor itself does not live here — it
//! lives in [`mod@crate::focus`], which owns the registry of mounted editors and
//! decides which one a key belongs to. This module is what happens once that
//! decision is made: [`handle_key`] drives one editor's model.
//!
//! The mapping in [`apply`] is intentionally small and Monaco-shaped: arrows and
//! their Ctrl/Shift variants move or extend, the edit keys insert and delete,
//! Ctrl+X/C/V drive the clipboard (see [`mod@crate::clipboard`]), and a bare
//! printable character types. Everything the editor does not claim returns
//! `false` so the runtime still sees it.
//!
//! The mouse side lives in [`mod@crate::mouse`]; the [`InputCtx`] bundle both
//! surfaces share is defined here.

use std::cell::RefCell;
use std::rc::Rc;

use rinch::core::{KeyEventData, KeyEventKind, Memo, NodeHandle, Signal};
use rinch_code_editor_model::{EditorModel, Grid, Motion, Selection, Selections};

use crate::autoscroll::AutoScroll;
use crate::clipboard;
use crate::focus::{self, EditorId};
use crate::mouse::{ClickCounter, GestureStart};
use crate::styles::LINE_HEIGHT;

/// Page Up/Down step when the viewport has not been laid out yet (its measured
/// height is 0 on the first frame); afterwards the step is the real count of
/// fully visible lines.
const FALLBACK_PAGE_LINES: usize = 20;

/// The live wiring both input surfaces (keyboard interceptor, mouse handler)
/// drive: the document, its render signals, the measured geometry, and the
/// scroll container. One bundle because the two handlers are facets of the same
/// input path and share almost all of it.
///
/// `after_op` runs after each consumed, state-changing operation, once the
/// render effects have flushed — the component uses it to reveal the caret and
/// clamp the scroll. It is a plain callback, not an effect, on purpose: it
/// writes the scroll signal, and a rinch signal write flushes subscribers
/// synchronously, so doing it from inside an effect re-enters the running
/// effect and panics ("RefCell already borrowed"). Input-handler context is the
/// one kind verified safe for writes that flush (it is where `version.update`
/// runs).
///
/// Its `bool` is Monaco's `minimalReveal`, and it splits by *who moved the
/// caret*: a mouse press or drag passes `true`, because you are pointing at the
/// place and shifting the view under your finger would be wrong; the keyboard
/// and edits pass `false` and get the horizontal padding, so typing towards the
/// right edge scrolls a little ahead of the caret. Monaco draws the same line,
/// in `coreCommands.ts`.
#[derive(Clone)]
pub(crate) struct InputCtx {
    /// Which editor this is, among all the ones mounted. Needed so a press can
    /// claim the keyboard away from its siblings — see [`mod@crate::focus`].
    pub id: EditorId,
    pub model: Rc<RefCell<EditorModel>>,
    pub grid: Grid,
    pub version: Signal<u64>,
    pub focused: Signal<bool>,
    pub cell_w: Memo<f32>,
    /// The document's scrollable width. Only the drag auto-scroller reads it —
    /// to clamp its own horizontal scrolling, since rinch has no horizontal
    /// layout clamp to fall back on.
    pub content_w: Memo<f32>,
    pub scroll_top: Signal<f64>,
    /// The live `stopRenderingLineAfter`. A press at or past it sets this to
    /// `usize::MAX`, which is how text a capped line does not draw becomes
    /// reachable at all.
    pub stop_after: Signal<usize>,
    pub view: NodeHandle,
    pub after_op: Rc<dyn Fn(bool)>,
    /// Restart the caret blink alone — everything `after_op` does except the
    /// caret reveal. For presses that take focus but claim nothing (a
    /// right-click), and for the auto-scroll tick, whose scrolling a reveal
    /// would fight (Monaco dispatches those with `RevealType::None`).
    pub blink_restart: Rc<dyn Fn()>,
    /// The multi-click state the mouse handler counts presses through.
    pub clicks: Rc<RefCell<ClickCounter>>,
    /// Monaco's `selectionStart`: the last gesture's range, persisted across
    /// presses so Shift+click extends around the clicked word or line.
    pub gesture: Rc<RefCell<Option<GestureStart>>>,
    /// The drag auto-scroller currently alive, if any — held so a new press
    /// and the component's unmount cleanup can stop it; `on_end` alone cannot
    /// be relied on (rinch's `Drag::cancel` drops it unfired).
    pub scroller: Rc<RefCell<Option<Rc<AutoScroll>>>>,
    /// Told after anything that changes the document or the selection, and
    /// after this editor gains or loses focus. The component points it at the
    /// `on_change` prop; a collaborative embedder is what listens.
    pub on_change: Rc<dyn Fn()>,
}

impl InputCtx {
    /// Announce that the document or the selection moved: repaint, then tell
    /// anyone listening.
    ///
    /// Every input path goes through here rather than bumping `version`
    /// itself. That is what keeps the two things that must happen on a change
    /// from drifting apart — and they had drifted: the drag auto-scroll tick
    /// bumped the version without running `after_op`, which was harmless while
    /// the only listener was the caret-reveal (which auto-scroll must *not*
    /// do), and stops being harmless the moment a collaborator is waiting to
    /// hear where this caret went.
    pub fn bump(&self) {
        self.version.update(|v| *v = v.wrapping_add(1));
        self.notify();
    }

    /// Tell the listener something changed, without repainting.
    ///
    /// Focus changes come through here: gaining or losing the keyboard alters
    /// nothing the version drives (the caret reads the focus signal directly),
    /// but it does change whether this editor has a caret worth publishing.
    pub fn notify(&self) {
        (self.on_change)();
    }

    /// Claim the keyboard for this editor, taking it from any sibling.
    pub fn take_focus(&self) {
        focus::focus(self.id);
    }
}

/// Drive one editor from one key event. Called by the shared interceptor in
/// [`mod@crate::focus`], which has already decided this editor owns the key.
pub(crate) fn handle_key(ctx: &InputCtx, ev: &KeyEventData) -> bool {
    // A page is the count of fully visible lines, measured fresh each press
    // so it tracks the window size for free (Monaco's pageSize).
    let page_lines = {
        let lines = (ctx.view.client_height() as f32 / LINE_HEIGHT) as usize;
        if lines > 0 {
            lines
        } else {
            FALLBACK_PAGE_LINES
        }
    };
    // Confine the model borrow to this block. Bumping `version` below runs
    // the reactive render effects synchronously, and those read the model —
    // so its borrow must be released first, or they panic on a double borrow.
    let outcome = {
        let mut m = ctx.model.borrow_mut();
        apply(&mut m, &ctx.grid, ev, page_lines)
    };
    match outcome {
        None => false,
        Some(changed) => {
            if changed {
                ctx.bump();
                // Keyboard and edits: the padded reveal.
                (ctx.after_op)(false);
            }
            true
        }
    }
}

/// Map one key event to a model operation. `None` means the editor does not
/// handle this key (the runtime keeps it); `Some(changed)` means it is consumed,
/// with `changed` telling whether the document or selection actually moved — an
/// edit that is a no-op (backspace at the very start) is consumed but changes
/// nothing, so it does not trigger a repaint.
fn apply(m: &mut EditorModel, grid: &Grid, ev: &KeyEventData, page_lines: usize) -> Option<bool> {
    // Every binding here acts on the press. The interceptor hears the release
    // too (rinch #337), spelled exactly like the press, so without this a
    // physical keystroke would be applied twice. Left unclaimed: the runtime
    // ignores a release's return value, and nothing else is owed it.
    if ev.kind == KeyEventKind::Up {
        return None;
    }
    // Ctrl combos first. On desktop `meta` is always false — Cmd arrives as ctrl
    // — so ctrl is the one modifier that carries shortcuts. Key strings arrive
    // lowercase from the desktop backend but verbatim (case follows Shift) from
    // the web one, hence the double patterns.
    if ev.ctrl && !ev.alt {
        match ev.key.as_str() {
            "z" | "Z" if ev.shift => return Some(m.redo().is_some()),
            "z" | "Z" => return Some(m.undo().is_some()),
            "y" | "Y" => return Some(m.redo().is_some()),
            "a" | "A" => {
                select_all(m);
                return Some(true);
            }
            // Clipboard. Copy is consumed but changes nothing on screen.
            // Ctrl+Shift+C/X stay unclaimed (Monaco leaves them too — the
            // browser wants them); Ctrl+Shift+V is paste-as-plain-text, which
            // in an unformatted editor is just paste.
            "c" | "C" if !ev.shift => {
                clipboard::copy(m);
                return Some(false);
            }
            "x" | "X" if !ev.shift => return Some(clipboard::cut(m).is_some()),
            "v" | "V" => return Some(clipboard::paste(m).is_some()),
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
        "PageUp" => moved(m, grid, Motion::Up(page_lines), extend),
        "PageDown" => moved(m, grid, Motion::Down(page_lines), extend),
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
        // rinch's desktop dispatch names the spacebar "Space" (the web backend
        // delivers " "); without this arm every physical space is dropped.
        "Space" => Some(m.insert(" ").is_some()),
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
/// Shared with the mouse side: four clicks select all, as Ctrl+A does.
pub(crate) fn select_all(m: &mut EditorModel) {
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

#[cfg(test)]
#[path = "input_tests.rs"]
mod tests;
