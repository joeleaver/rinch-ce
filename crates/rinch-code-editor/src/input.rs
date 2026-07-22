//! Input: turning key and mouse events into model operations.
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
//!
//! The mouse side is [`text_mousedown`]: the `onmousedown` handler for the layer
//! stack that places the caret at the click (Shift extends), Monaco's midpoint
//! snapping included via the grid's hit-testing.

use std::cell::RefCell;
use std::rc::Rc;

use rinch::core::{
    Drag, KeyEventData, Memo, MouseButton, NodeHandle, Signal, get_click_context,
    set_keyboard_interceptor,
};
use rinch_code_editor_model::{EditorModel, Grid, Motion, Position, Selection, Selections};

use crate::styles::{LINE_HEIGHT, TEXT_PAD_LEFT};

/// Page Up/Down step when the viewport has not been laid out yet (its measured
/// height is 0 on the first frame); afterwards the step is the real count of
/// fully visible lines.
const FALLBACK_PAGE_LINES: usize = 20;

/// Width of the interactive strip rinch reserves at the right edge of a scroll
/// container whose content overflows (see rinch's `find_scrollbar_hit`): a press
/// there grabs the scrollbar, and since rinch dispatches `data-onmousedown`
/// additively *before* the scrollbar logic, the caret handler has to step aside
/// itself — Monaco likewise never moves the cursor on a scrollbar grab.
const SCROLLBAR_HIT_WIDTH: f32 = 16.0;

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
#[derive(Clone)]
pub(crate) struct InputCtx {
    pub model: Rc<RefCell<EditorModel>>,
    pub grid: Grid,
    pub version: Signal<u64>,
    pub focused: Signal<bool>,
    pub cell_w: Memo<f32>,
    pub scroll_top: Signal<f64>,
    pub view: NodeHandle,
    pub after_op: Rc<dyn Fn()>,
}

/// Install the global keyboard interceptor. It drives the model, bumps the
/// version so the reactive render repaints, and does nothing unless `focused`
/// is set.
pub(crate) fn install(ctx: InputCtx) {
    set_keyboard_interceptor(move |ev| {
        if !ctx.focused.get() {
            return false;
        }
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
                    ctx.version.update(|v| *v = v.wrapping_add(1));
                    (ctx.after_op)();
                }
                true
            }
        }
    });
}

/// The char offset under a document-space point: the line under `y` (clamped
/// into the document), then the nearest character boundary to `x` on it —
/// Monaco's midpoint rule, via the grid. `x` is pixels from the glyph origin;
/// `cell_w` is the measured cell advance. `line_text` carries no terminator, so
/// a point past the end of a line lands at the line end for free.
fn hit_offset(m: &EditorModel, grid: &Grid, cell_w: f32, x: f32, y: f32) -> usize {
    let line = grid.line_at_y(y, m.len_lines());
    let text = m.buffer().line_text(line);
    let col = grid.char_column_at_visual(&text, x / cell_w);
    m.buffer().offset_of(Position::new(line, col))
}

/// Build the `onmousedown` handler for the layer stack: place the caret at the
/// pressed position (Shift extends the primary selection there instead), then
/// capture the pointer for drag selection until mouseup. Also takes focus, so a
/// press anywhere in the text starts routing keys here.
///
/// The handler element is the document-sized layer stack, which scrolls with the
/// text, and rinch's mouse dispatch subtracts ancestor scroll offsets when it
/// computes the handler element's bounds — so the element-relative press
/// position *is* the document position, with no scroll arithmetic at all.
///
/// The drag goes through rinch's `Drag` pointer capture: `on_move` keeps firing
/// with viewport coordinates wherever the pointer goes (over the gutter, off the
/// editor) until mouseup lands anywhere, so there are no dead zones and no stuck
/// drags. Two coordinate subtleties in the capture:
///
/// - The layer stack's bounds go stale the moment the drag scrolls (it moves
///   with the text), so the drag captures the *scroll container's* viewport
///   origin instead — `element_y + scroll_top` — which holds still while the
///   document slides under it; each move then adds the live `scroll_top` back.
/// - Extending to a line outside the viewport reveals it (`after_op`), which
///   scrolls the view — so dragging at the edge advances line by line as the
///   pointer moves. Continuous timer-driven auto-scroll while the pointer rests
///   past the edge is Monaco behavior deferred to the polish increment.
pub(crate) fn text_mousedown(ctx: InputCtx) -> impl Fn() {
    move || {
        ctx.focused.set(true);
        let click = get_click_context();
        if click.button != MouseButton::Left {
            return;
        }
        // Step aside for a scrollbar grab (the strip only exists when the
        // document overflows the viewport; the layer stack's height is the
        // document's).
        let viewport = ctx.view.client_height() as f32;
        if viewport > 0.0
            && click.element_height > viewport
            && click.mouse_x >= click.element_x + click.element_width - SCROLLBAR_HIT_WIDTH
        {
            return;
        }
        // Confine the borrow, as in the interceptor: the version bump below
        // flushes render effects that borrow the model.
        {
            let mut m = ctx.model.borrow_mut();
            let offset = hit_offset(
                &m,
                &ctx.grid,
                ctx.cell_w.get(),
                click.mouse_x - click.element_x - TEXT_PAD_LEFT,
                click.mouse_y - click.element_y,
            );
            if click.modifiers.shift {
                let anchor = m.selections().primary().anchor;
                m.set_selections(Selections::single(Selection::new(anchor, offset)));
            } else {
                m.set_cursor(offset);
            }
        }
        // Repaint unconditionally: even a press on the caret's own cell can
        // collapse a selection or a multi-cursor set, and a redundant repaint of
        // an unchanged frame is cheap next to detecting one.
        ctx.version.update(|v| *v = v.wrapping_add(1));
        (ctx.after_op)();

        // Capture the pointer for the selection drag. The anchor is re-read from
        // the model on every move (not captured here) so the drag keeps working
        // off the live primary selection — this is also what makes Shift+press
        // followed by a drag extend from the original anchor, as Monaco does.
        let origin_x = click.element_x + TEXT_PAD_LEFT;
        let view_top = click.element_y + ctx.scroll_top.get() as f32;
        let drag = ctx.clone();
        Drag::absolute()
            .on_move(move |mx, my| {
                let changed = {
                    let mut m = drag.model.borrow_mut();
                    let doc_y = my - view_top + drag.scroll_top.get() as f32;
                    let offset =
                        hit_offset(&m, &drag.grid, drag.cell_w.get(), mx - origin_x, doc_y);
                    let primary = *m.selections().primary();
                    // Moves within the same cell change nothing — skip the
                    // repaint; mousemove arrives per pixel, cells are ~8px.
                    if offset == primary.head {
                        false
                    } else {
                        m.set_selections(Selections::single(Selection::new(
                            primary.anchor,
                            offset,
                        )));
                        true
                    }
                };
                if changed {
                    drag.version.update(|v| *v = v.wrapping_add(1));
                    (drag.after_op)();
                }
            })
            .start();
    }
}

/// Map one key event to a model operation. `None` means the editor does not
/// handle this key (the runtime keeps it); `Some(changed)` means it is consumed,
/// with `changed` telling whether the document or selection actually moved — an
/// edit that is a no-op (backspace at the very start) is consumed but changes
/// nothing, so it does not trigger a repaint.
fn apply(m: &mut EditorModel, grid: &Grid, ev: &KeyEventData, page_lines: usize) -> Option<bool> {
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
