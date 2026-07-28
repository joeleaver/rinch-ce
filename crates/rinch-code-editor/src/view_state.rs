//! The derived values the render reads, as memos.
//!
//! Each of these captures the model (or a node handle) **once** and recomputes
//! when the document version bumps. That shape is what lets the reactive style
//! closures in the component read them at all: a [`Memo`] is `Copy`, and the
//! rsx macro re-evaluates a reactive `style:` block on every effect run, so
//! anything it captures must survive being captured again. A cloned model or
//! node handle would not.
//!
//! # Two measurements taken synchronously, on purpose
//!
//! [`ViewState::cell_w`] reads the probe's `client_width` directly rather than
//! subscribing to its `bounds_signal`, and [`ViewState::visible`] does the same
//! with the viewport's `client_height`. A bounds-signal subscriber re-runs
//! *during paint*, inside the document's read borrow (rinch's
//! `update_bounds_signals`), and setting a style attribute there re-enters that
//! borrow and panics. Keying the measurement on the version instead means it
//! re-runs from the input path, where the document is free — and neither
//! measurement depends on the document's contents anyway. It is also the
//! pattern rinch's own `virtual_list` uses.
//!
//! Before the first layout both measurements read 0, and the fallbacks stand in
//! for exactly one frame.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

use rinch::core::{Memo, NodeHandle, Signal};
use rinch_code_editor_model::{EditorModel, Grid, max_visual_columns};

use crate::render;
use crate::styles::{
    FALLBACK_CELL_WIDTH, OVERFLOW_CHIP_CELLS, PROBE_LEN, SCROLL_BEYOND_LAST_COLUMN, TEXT_PAD_LEFT,
};
use crate::{FALLBACK_VIEWPORT_H, OVERSCAN};

/// Everything the render derives from the document and the scroll position.
///
/// `Copy`, because every field is — which is what lets the component hand these
/// straight into reactive closures.
#[derive(Clone, Copy)]
pub(crate) struct ViewState {
    /// The measured advance of one monospace cell, in pixels.
    pub cell_w: Memo<f32>,
    /// The document's full pixel height; the layer stack is sized to it.
    pub content_h: Memo<f32>,
    /// The scrollable pixel width — Monaco's `scrollWidth`. See
    /// [`ViewState::new`] for why it only ever grows.
    pub content_w: Memo<f32>,
    /// The line the primary caret is on — what the current-line band highlights.
    pub current_line: Memo<usize>,
    /// The primary caret's `(line, visual_column)`.
    pub caret: Memo<(usize, usize)>,
    /// The slice of lines the rows materialize, plus overscan.
    pub visible: Memo<Range<usize>>,
}

impl ViewState {
    pub(crate) fn new(
        model: &Rc<RefCell<EditorModel>>,
        grid: Grid,
        version: Signal<u64>,
        scroll_top: Signal<f64>,
        stop_after: Signal<usize>,
        probe: NodeHandle,
        view: NodeHandle,
    ) -> Self {
        let cell_w = Memo::new(move || {
            version.get();
            let w = probe.client_width() as f32;
            if w > 0.0 {
                w / PROBE_LEN as f32
            } else {
                FALLBACK_CELL_WIDTH
            }
        });
        let content_h = Memo::new({
            let m = model.clone();
            move || {
                version.get();
                render::content_height(&m.borrow())
            }
        });
        let current_line = Memo::new({
            let m = model.clone();
            move || {
                version.get();
                render::caret_line(&m.borrow())
            }
        });
        let caret = Memo::new({
            let m = model.clone();
            move || {
                version.get();
                render::caret_grid(&m.borrow(), &grid, stop_after.get())
            }
        });
        // Recomputes on edit (`version`) and on scroll (`scroll_top`), which is
        // what re-slices the window as the viewport moves.
        let visible = Memo::new({
            let m = model.clone();
            move || {
                version.get();
                let st = scroll_top.get() as f32;
                let total = m.borrow().len_lines();
                let measured = view.client_height() as f32;
                let viewport = if measured > 1.0 {
                    measured
                } else {
                    FALLBACK_VIEWPORT_H
                };
                grid.visible_lines(st, viewport, total, OVERSCAN)
            }
        });

        // The horizontal scroll range, folded the way Monaco folds it: a
        // maximum over the lines currently materialized, which only ever grows.
        //
        // Monaco does this because it cannot measure a line it has not put in
        // the DOM. We could measure any line — see the note in the model
        // crate's `width` module for why we follow it anyway. The visible
        // consequence, in Monaco and here, is that the horizontal scroll range
        // grows as you scroll down past a long line and does not shrink when
        // you scroll back; the mark resets only when the whole document is on
        // screen at once, which is Monaco's own reset condition.
        //
        // The mark is kept in **visual columns**, not pixels. Monaco stores
        // pixels and resets on a font change; columns are independent of the
        // cell advance, so the first frame's fallback width cannot strand a
        // too-wide mark when the real measurement arrives.
        let content_w = Memo::new({
            let m = model.clone();
            let widest = Rc::new(Cell::new(0usize));
            move || {
                version.get();
                let range = visible.get();
                let cw = cell_w.get();
                let cols = {
                    let mb = m.borrow();
                    let whole_document = range.start == 0 && range.end >= mb.len_lines();
                    // The *live* cap, not the constant: a line the user has
                    // uncapped is drawn in full, so the document is that wide.
                    let seen = max_visual_columns(mb.buffer(), &grid, range, stop_after.get());
                    // A truncated line ends in a chip, which is laid out past
                    // the last drawn cell and has to stay inside the scroll
                    // range — it is the only way back to the hidden text.
                    let width = seen.columns + if seen.capped { OVERFLOW_CHIP_CELLS } else { 0 };
                    if whole_document {
                        width
                    } else {
                        width.max(widest.get())
                    }
                };
                widest.set(cols);
                // Monaco's `_computeContentWidth`, less its vertical-scrollbar
                // term: on web `client_width` already excludes the scrollbar,
                // and desktop paints its own as an overlay.
                //
                // Monaco's other half — `scrollWidth = max(viewportWidth,
                // contentWidth)` — is deliberately *not* applied here. It needs
                // the viewport width, which measures 0 before the first layout,
                // and this memo has nothing that would recompute it afterwards.
                // `layers_style` expresses that maximum as `min-width:100%` and
                // lets layout resolve it instead. The consequence for callers is
                // that this value can be *narrower* than the viewport, which is
                // exactly what the horizontal clamp in `scroll.rs` wants — a
                // document that fits has a maximum scroll of zero.
                TEXT_PAD_LEFT + (cols + SCROLL_BEYOND_LAST_COLUMN) as f32 * cw
            }
        });

        Self {
            cell_w,
            content_h,
            content_w,
            current_line,
            caret,
            visible,
        }
    }
}
