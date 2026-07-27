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

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use rinch::core::{Memo, NodeHandle, Signal};
use rinch_code_editor_model::{EditorModel, Grid};

use crate::render;
use crate::styles::{FALLBACK_CELL_WIDTH, PROBE_LEN};
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
                render::caret_grid(&m.borrow(), &grid)
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

        Self {
            cell_w,
            content_h,
            current_line,
            caret,
            visible,
        }
    }
}
