//! Mouse input: caret placement, multi-click gestures, and drag selection.
//!
//! [`text_mousedown`] builds the `onmousedown` handler for the layer stack —
//! document-sized and scrolling with the text, so rinch's dispatch delivers
//! the press in document coordinates with no scroll arithmetic (see the
//! handler's comments). What happens then depends on the click count:
//!
//! - one click places the caret (Shift extends), and dragging extends by
//!   character;
//! - two clicks select the word under the pointer, and dragging extends by
//!   whole words;
//! - three clicks select the line, and dragging extends by whole lines;
//! - four or more select the whole document.
//!
//! The counting is Monaco's `MouseDownState`, not the platform's: rinch (like
//! winit) reports no click count, so [`ClickCounter`] rebuilds it — a count
//! that grows by one per press, resets after 400 ms of quiet
//! (`CLEAR_MOUSE_DOWN_COUNT_TIME`) or when the press lands away from the last
//! one (the OS double-click radius Monaco inherits from the browser's
//! `detail`), and is capped by how many consecutive presses hit the *same
//! logical position* (the same character offset). The gesture semantics
//! themselves (which word, how the anchor flips) live rinch-free in the model
//! crate's select module; this module routes coordinates in and selections
//! out.
//!
//! # The stored gesture range — Monaco's `selectionStart`
//!
//! Monaco's cursor keeps the originally clicked *range* — a collapsed point
//! for a plain click, the whole word or line for a gesture — and every
//! Shift+press and every drag move extends from it, the anchor flipping to
//! whichever of its ends stays behind the head. That is what keeps a
//! double-clicked word entirely inside a later backward Shift+extension.
//! [`GestureStart`] persists that range across presses, remembering the
//! selection it produced; if the selection has since moved (arrow keys, an
//! edit), the store is stale and a Shift+press falls back to extending from
//! the live anchor — which is exactly what Monaco's cursor does too, since
//! any cursor operation resets its `selectionStart`.
//!
//! The drag runs under rinch's `Drag::absolute()` pointer capture, one mode
//! for the whole drag — Monaco keeps the mouse-down count for the duration,
//! so a double-click drag stays word-wise wherever the pointer goes.

use std::ops::Range;
use std::rc::Rc;

use rinch::core::{Drag, MouseButton, get_click_context};
use rinch_code_editor_model::{
    EditorModel, Grid, Position, Selection, Selections, extend_from, line_drag, line_range_at,
    word_drag, word_range_at,
};
use web_time::Instant;

use crate::autoscroll::AutoScroll;
use crate::input::{InputCtx, select_all};
use crate::styles::{SCROLLBAR_WIDTH, TEXT_PAD_LEFT};

/// Monaco's `CLEAR_MOUSE_DOWN_COUNT_TIME`: presses further apart than this
/// restart the click count at one.
const MULTI_CLICK_RESET_MS: u128 = 400;

/// Presses further apart than this on screen restart the count too — the
/// piece of the platform's double-click detection Monaco gets for free from
/// the browser's `detail` (the OS radius) and we rebuild; rinch's own
/// detector uses the same 5 px.
const MULTI_CLICK_RADIUS: f32 = 5.0;

/// Monaco's `MouseDownState`, rebuilt because the platform gives us no click
/// count. See the module docs for the rules.
pub(crate) struct ClickCounter {
    last_time: Option<Instant>,
    last_point: (f32, f32),
    last_offset: usize,
    equal_presses: u32,
    count: u32,
}

impl ClickCounter {
    pub(crate) fn new() -> Self {
        Self {
            last_time: None,
            last_point: (0.0, 0.0),
            last_offset: 0,
            equal_presses: 0,
            count: 0,
        }
    }

    /// Record a press at logical `offset` and viewport point `(x, y)`, and
    /// return the click count it makes.
    fn register(&mut self, offset: usize, x: f32, y: f32) -> u32 {
        let now = Instant::now();
        let timed_out = self
            .last_time
            .is_none_or(|t| now.duration_since(t).as_millis() > MULTI_CLICK_RESET_MS);
        let moved_away = {
            let (lx, ly) = self.last_point;
            (x - lx).hypot(y - ly) > MULTI_CLICK_RADIUS
        };
        let incoming = if timed_out || moved_away {
            1
        } else {
            self.count + 1
        };
        // The same-position streak is tracked independently of the timing,
        // as in Monaco — a slow re-click on the same spot keeps the streak
        // but still resets the count.
        if self.last_time.is_some() && self.last_offset == offset {
            self.equal_presses += 1;
        } else {
            self.equal_presses = 1;
        }
        self.last_time = Some(now);
        self.last_point = (x, y);
        self.last_offset = offset;
        self.count = incoming.min(self.equal_presses);
        self.count
    }
}

/// Monaco's `selectionStart`, persisted across presses: the range the last
/// gesture started from, and the selection it left behind — the staleness
/// check that stands in for Monaco resetting its `selectionStart` on every
/// non-mouse cursor operation.
pub(crate) struct GestureStart {
    pub(crate) range: Range<usize>,
    pub(crate) primary: (usize, usize),
}

/// What kind of selection this drag extends, fixed at the press, each
/// carrying the gesture range it extends from. Shared with the auto-scroller,
/// which keeps extending in the same mode while the pointer rests past the
/// viewport edge.
pub(crate) enum DragMode {
    Char(Range<usize>),
    Word(Range<usize>),
    Line(Range<usize>),
}

impl DragMode {
    pub(crate) fn range(&self) -> &Range<usize> {
        match self {
            DragMode::Char(r) | DragMode::Word(r) | DragMode::Line(r) => r,
        }
    }

    /// The selection this drag mode produces with its head at `offset`.
    pub(crate) fn extend(&self, m: &EditorModel, offset: usize) -> Selection {
        match self {
            DragMode::Char(initial) => extend_from(initial, offset),
            DragMode::Word(initial) => word_drag(m.buffer(), initial, offset),
            DragMode::Line(initial) => line_drag(m.buffer(), initial, offset),
        }
    }
}

/// The char offset under a document-space point: the line under `y` (clamped
/// into the document), then the nearest character boundary to `x` on it —
/// Monaco's midpoint rule, via the grid. `x` is pixels from the glyph origin;
/// `cell_w` is the measured cell advance. `line_text` carries no terminator, so
/// a point past the end of a line lands at the line end for free.
pub(crate) fn hit_offset(m: &EditorModel, grid: &Grid, cell_w: f32, x: f32, y: f32) -> usize {
    let line = grid.line_at_y(y, m.len_lines());
    let text = m.buffer().line_text(line);
    let col = grid.char_column_at_visual(&text, x / cell_w);
    m.buffer().offset_of(Position::new(line, col))
}

/// Build the `onmousedown` handler for the layer stack. Applies the gesture
/// for the press's click count, then captures the pointer so the drag keeps
/// extending in the same mode until mouseup lands anywhere.
///
/// The handler element is the document-sized layer stack, which scrolls with the
/// text, and rinch's mouse dispatch subtracts ancestor scroll offsets when it
/// computes the handler element's bounds — so the element-relative press
/// position *is* the document position, with no scroll arithmetic at all.
///
/// Two coordinate subtleties in the capture (unchanged from the plain drag):
///
/// - The layer stack's bounds go stale the moment the drag scrolls (it moves
///   with the text), so the drag captures the *scroll container's* viewport
///   origin instead — `element_y + scroll_top` at press — which holds still
///   while the document slides under it; each move adds the live `scroll_top`
///   back.
/// - A move outside the viewport does not extend directly: it feeds the
///   [`AutoScroll`] timer, which scrolls at Monaco's distance-based speed and
///   extends to the edge line each tick until the pointer comes back in (or
///   the drag ends).
pub(crate) fn text_mousedown(ctx: InputCtx) -> impl Fn() {
    move || {
        // Take focus and arm the blink *before* the early returns below: a
        // right-click or a scrollbar grab focuses the editor too, and a
        // focused caret with no blink timer would sit solid forever. Claiming
        // focus takes it from any sibling editor, which is also how that
        // sibling learns it has been left.
        ctx.take_focus();
        (ctx.blink_restart)();
        let click = get_click_context();
        if click.button != MouseButton::Left {
            return;
        }
        // Step aside for a scrollbar grab (the strip only exists when the
        // document overflows the viewport; the layer stack's height is the
        // document's).
        //
        // The strip is measured from the *scroll container's* right edge, not
        // the layer stack's. They used to coincide — the stack had no width, so
        // it laid out exactly as wide as its container. Now that it is the
        // document's width, the stack's right edge can be tens of thousands of
        // pixels away, and measuring from it would put the guard off-screen and
        // let a scrollbar grab drop a caret.
        let viewport = ctx.view.client_height() as f32;
        let view_right =
            click.element_x + ctx.view.scroll_left() as f32 + ctx.view.client_width() as f32;
        if viewport > 0.0
            && click.element_height > viewport
            && click.mouse_x >= view_right - SCROLLBAR_WIDTH
        {
            return;
        }
        // Monaco's `LongLinesHelper`: a press at or past the cap lifts it, for
        // this editor, permanently. It keys on the *column*, not on hitting the
        // chip element — which is why the chip needs no handler of its own, and
        // why pressing in the blank space past a capped line's end works too.
        //
        // Decided inside the model borrow, applied outside it: writing the
        // signal flushes the render effects, and those borrow the model.
        let uncap;

        // Confine the borrow: the version bump below flushes render effects
        // that borrow the model.
        let mode = {
            let mut m = ctx.model.borrow_mut();
            let offset = hit_offset(
                &m,
                &ctx.grid,
                ctx.cell_w.get(),
                click.mouse_x - click.element_x - TEXT_PAD_LEFT,
                click.mouse_y - click.element_y,
            );
            let cap = ctx.stop_after.get();
            uncap = cap != usize::MAX && m.buffer().position_of(offset).col >= cap;

            let count = ctx
                .clicks
                .borrow_mut()
                .register(offset, click.mouse_x, click.mouse_y);

            // The range a Shift+press extends from: the stored gesture range
            // while it is still current, else collapsed at the live anchor
            // (Monaco's selectionStart after any non-mouse cursor operation).
            let shift_initial = |m: &EditorModel| -> Range<usize> {
                let primary = *m.selections().primary();
                match &*ctx.gesture.borrow() {
                    Some(g) if g.primary == (primary.anchor, primary.head) => g.range.clone(),
                    _ => primary.anchor..primary.anchor,
                }
            };

            let (initial, sel) = match count {
                1 => {
                    let initial = if click.modifiers.shift {
                        shift_initial(&m)
                    } else {
                        offset..offset
                    };
                    (initial.clone(), extend_from(&initial, offset))
                }
                2 => {
                    let initial = if click.modifiers.shift {
                        shift_initial(&m)
                    } else {
                        word_range_at(m.buffer(), offset)
                    };
                    (initial.clone(), word_drag(m.buffer(), &initial, offset))
                }
                3 => {
                    let initial = if click.modifiers.shift {
                        shift_initial(&m)
                    } else {
                        line_range_at(m.buffer(), offset)
                    };
                    (initial.clone(), line_drag(m.buffer(), &initial, offset))
                }
                _ => {
                    // Monaco selects all and keeps re-selecting all on drag
                    // moves; the second half is a no-op, so no capture. The
                    // model borrow must end before the version bump — the
                    // render effects it flushes borrow the model.
                    select_all(&mut m);
                    let end = m.len_chars();
                    *ctx.gesture.borrow_mut() = Some(GestureStart {
                        range: 0..end,
                        primary: (0, end),
                    });
                    drop(m);
                    if uncap {
                        ctx.stop_after.set(usize::MAX);
                    }
                    ctx.bump();
                    (ctx.after_op)(true);
                    return;
                }
            };
            m.set_selections(Selections::single(sel));
            *ctx.gesture.borrow_mut() = Some(GestureStart {
                range: initial.clone(),
                primary: (sel.anchor, sel.head),
            });
            match count {
                1 => DragMode::Char(initial),
                2 => DragMode::Word(initial),
                _ => DragMode::Line(initial),
            }
        };
        // Repaint unconditionally: even a press on the caret's own cell can
        // collapse a selection or a multi-cursor set, and a redundant repaint of
        // an unchanged frame is cheap next to detecting one.
        if uncap {
            ctx.stop_after.set(usize::MAX);
        }
        ctx.bump();
        // Mouse-driven: reveal the caret exactly, with no padding. You are
        // pointing at the place; shifting the view under your finger is wrong.
        (ctx.after_op)(true);

        // Capture the pointer for the selection drag. Every mode extends from
        // its gesture range — Monaco's per-move `_computeSelection` against
        // the stored selectionStart — so a drag after Shift+press keeps the
        // original range inside the selection, whichever way it goes. A move
        // past the viewport's top or bottom hands off to the auto-scroller
        // instead of extending; re-entering hands back.
        // The layer stack's own bounds go stale the moment the drag scrolls it,
        // on either axis, so both origins are captured as the *scroll
        // container's* viewport edges — `element + scroll` at the press — which
        // hold still while the document slides under them. Every later use adds
        // the live scroll offset back.
        //
        // The horizontal term is easy to leave out and impossible to notice
        // until something scrolls sideways mid-drag: without it every hit-test
        // after the first horizontal auto-scroll tick is off by the distance
        // scrolled, and the selection runs away from the pointer.
        let view_left = click.element_x + ctx.view.scroll_left() as f32;
        let view_top = click.element_y + ctx.scroll_top.get() as f32;
        let origin_x = view_left + TEXT_PAD_LEFT;
        let mode = Rc::new(mode);
        let scroller = AutoScroll::new(ctx.clone(), mode.clone(), origin_x, view_left, view_top);
        // Park the scroller where a later press and the unmount cleanup can
        // reach it; stop any predecessor a lost `on_end` left behind.
        if let Some(old) = ctx.scroller.borrow_mut().replace(scroller.clone()) {
            old.stop();
        }
        let drag = ctx.clone();
        let end_scroller = scroller.clone();
        let end_slot = ctx.scroller.clone();
        Drag::absolute()
            .on_move(move |mx, my| {
                let viewport_h = drag.view.client_height() as f32;
                let viewport_w = drag.view.client_width() as f32;
                let scroll_left = drag.view.scroll_left() as f32;
                let outside = (viewport_h > 0.0 && (my < view_top || my > view_top + viewport_h))
                    || (viewport_w > 0.0 && (mx < view_left || mx > view_left + viewport_w));
                if outside {
                    scroller.update_pointer(mx, my);
                    return;
                }
                scroller.stop();
                let changed = {
                    let mut m = drag.model.borrow_mut();
                    let doc_y = my - view_top + drag.scroll_top.get() as f32;
                    let offset = hit_offset(
                        &m,
                        &drag.grid,
                        drag.cell_w.get(),
                        mx - origin_x + scroll_left,
                        doc_y,
                    );
                    let primary = *m.selections().primary();
                    // Moves within the same cell change nothing on a char
                    // drag — skip even computing; mousemove arrives per
                    // pixel, cells are ~8px.
                    if matches!(&*mode, DragMode::Char(_)) && offset == primary.head {
                        false
                    } else {
                        let s = mode.extend(&m, offset);
                        if s.anchor != primary.anchor || s.head != primary.head {
                            m.set_selections(Selections::single(s));
                            // Keep the stored gesture current through the
                            // drag, so a Shift+press right after keeps
                            // extending from the same range.
                            *drag.gesture.borrow_mut() = Some(GestureStart {
                                range: mode.range().clone(),
                                primary: (s.anchor, s.head),
                            });
                            true
                        } else {
                            false
                        }
                    }
                };
                if changed {
                    drag.bump();
                    (drag.after_op)(true);
                }
            })
            .on_end(move |_, _| {
                end_scroller.stop();
                end_slot.borrow_mut().take();
            })
            .start();
    }
}
