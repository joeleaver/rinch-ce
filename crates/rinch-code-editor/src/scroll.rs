//! Keeping the viewport and the `scroll_top` signal telling the same story, and
//! bringing the caret back into view on both axes.
//!
//! The text scrolls natively — its container has `overflow:auto` and the browser
//! or the desktop layout engine moves it — while a `scroll_top` signal mirrors
//! that offset for everything laid out beside the text: the visible-line window,
//! the gutter band. Two things can put the mirror out of step with the DOM, and
//! [`reveal_and_clamp`] handles both in one adjustment.
//!
//! # There is no `scroll_left` signal, deliberately
//!
//! `scroll_top` is mirrored because the *render* reads it: the visible-line
//! window and the gutter band are both derived from it. Nothing in the render
//! reads the horizontal offset — rows, carets, selection rectangles and the
//! current-line band are all positioned in document coordinates inside the
//! layer stack, which is itself what scrolls, so native scrolling places them
//! with no arithmetic from us.
//!
//! That is fortunate, because a horizontal mirror could not be kept honest.
//! rinch never reports a horizontal scroll: on desktop the wheel's horizontal
//! branch moves the container and repaints without ever firing `data-onscroll`,
//! and the scroll callback carries a single `f64` documented as the vertical
//! offset, so even the web side — where the event does fire — reports the wrong
//! axis. A mirrored signal would silently drift on the first trackpad swipe.
//!
//! So the rule here is: **the scroll container is the only source of truth for
//! the horizontal offset.** Read it fresh with `scroll_left()` at the point of
//! use, write it with `set_scroll_left()`, never cache it. Both call sites (this
//! one and the drag auto-scroller) run in event-handler context, where reading
//! the DOM is free and safe.
//!
//! # Two clamps, and only one of them is rinch's
//!
//! The vertical clamp below mirrors one rinch performs silently during layout.
//! There is no horizontal equivalent in rinch — `clamp_scroll_offsets` filters
//! on `overflow-y` and writes only the vertical offset — which cuts both ways:
//! nothing will strand our value behind our back, and equally nothing will
//! rescue an out-of-range one. The horizontal clamp here is therefore ours
//! alone, and it must run before every write.

use std::cell::RefCell;
use std::rc::Rc;

use rinch::core::{Memo, NodeHandle, Signal};
use rinch_code_editor_model::{EditorModel, Grid};

use crate::render;
use crate::styles::{
    HORIZONTAL_EXTRA_PX, LINE_HEIGHT, REVEAL_HORIZONTAL_RIGHT_PADDING, SCROLLBAR_WIDTH,
    TEXT_PAD_LEFT,
};

/// Monaco's `ViewLines._computeMinimumScrolling`: the smallest scroll offset
/// that brings `[box_start, box_end]` inside `[viewport_start, viewport_end]`,
/// or the viewport's current start if it is already there.
///
/// Both axes use it, which is the point of lifting it out — the vertical path
/// had this shape open-coded, and the horizontal path must not be allowed to
/// drift into a different rule.
///
/// Monaco's two optional `revealAtStart` / `revealAtEnd` flags are omitted:
/// they serve its reveal *strategies* (reveal-in-centre and friends), and this
/// editor has only the plain "bring it into view" one. The box-does-not-fit
/// branch is kept even though a single caret box can only hit it in a viewport
/// narrower than the caret's own padding, because it is what a future
/// multi-line or wide-selection reveal will need and it is one line.
fn minimum_scrolling(viewport_start: f32, viewport_end: f32, box_start: f32, box_end: f32) -> f32 {
    let viewport_length = viewport_end - viewport_start;
    if box_end - box_start < viewport_length {
        if box_start < viewport_start {
            box_start
        } else if box_end > viewport_end {
            (box_end - viewport_length).max(0.0)
        } else {
            viewport_start
        }
    } else {
        // Cannot fit: show its beginning.
        box_start
    }
}

/// Build the after-every-operation scroll adjustment: bring the caret's line
/// back into the viewport when a move or an edit pushed it out, bring its
/// *column* back likewise, and clamp the vertical scroll when the document
/// shrank.
///
/// The returned closure takes Monaco's `minimalReveal`. A mouse-driven caret
/// move passes `true` and reveals the caret exactly, with no padding — you are
/// pointing at the place, so nudging the view under your finger is wrong. A
/// keyboard move or an edit passes `false` and gets Monaco's asymmetric
/// breathing room, so typing towards the right edge scrolls a little ahead of
/// the caret rather than pinning it to the edge.
///
/// The clamp is the less obvious half. rinch's layout clamps a scroll
/// container's DOM offset silently when the content shrinks under it (undo, a
/// deletion) — the clamp runs inside layout resolution, where firing `onscroll`
/// is unsafe — which would strand `scroll_top`, and with it the gutter band and
/// the visible-line window, at an offset the DOM no longer has. Mirroring the
/// clamp here keeps the two in step, and is exact: the viewport height does not
/// change on an edit, so recomputing rinch's own max-scroll gives the same
/// number rinch computed.
///
/// # This must not become an effect
///
/// The returned closure writes `scroll_top`, and a rinch signal write flushes
/// its subscribers synchronously; a flush from inside a running effect re-enters
/// that effect and panics ("RefCell already borrowed"). So this is called as a
/// plain callback from input-handler context — the keyboard interceptor and the
/// mouse handlers — which is the context already verified safe for writes that
/// flush (it is where `version.update` runs). A window *resize* can still clamp
/// silently, because rinch fires no resize event; the blink tick's reconciler
/// covers that gap. See [`mod@crate::blink`].
pub(crate) fn reveal_and_clamp(
    view: NodeHandle,
    model: Rc<RefCell<EditorModel>>,
    grid: Grid,
    cell_w: Memo<f32>,
    scroll_top: Signal<f64>,
    stop_after: Signal<usize>,
    content_w: Memo<f32>,
) -> impl Fn(bool) + 'static {
    move |minimal: bool| {
        let viewport = view.client_height() as f32;
        if viewport <= 0.0 {
            return;
        }
        let ((line, vcol), content) = {
            let mb = model.borrow();
            (
                render::caret_grid(&mb, &grid, stop_after.get()),
                render::content_height(&mb),
            )
        };

        // ---- vertical ----------------------------------------------------
        let st = scroll_top.get() as f32;
        let top = line as f32 * LINE_HEIGHT;
        let revealed = minimum_scrolling(st, st + viewport, top, top + LINE_HEIGHT);
        let new_st = revealed.min((content - viewport).max(0.0)).max(0.0);
        if new_st != st {
            view.set_scroll_top(new_st as f64);
            scroll_top.set(new_st as f64);
        }

        // ---- horizontal --------------------------------------------------
        //
        // Strictly after the vertical half: `content_w` is folded over the
        // visible line window, so it must be read once that window reflects the
        // line we just revealed — otherwise the clamp below can be computed
        // against a document width that does not yet include the caret's own
        // line.
        let client_w = view.client_width() as f32;
        if client_w <= 0.0 {
            return;
        }
        let x = TEXT_PAD_LEFT + vcol as f32 * cell_w.get();
        let (box_start, box_end) = if minimal {
            (x, x)
        } else {
            (
                (x - HORIZONTAL_EXTRA_PX).max(0.0),
                x + REVEAL_HORIZONTAL_RIGHT_PADDING,
            )
        };
        // The viewport ends where the scrollbar overlay begins, not at the
        // container's edge: rinch paints that strip *over* the content, so a
        // caret revealed into it is a caret you cannot see. Monaco subtracts its
        // `verticalScrollbarWidth` here for the same reason. Only subtract it
        // when there is a scrollbar to subtract.
        let sl = view.scroll_left() as f32;
        let overlaid = if view.scroll_height() > view.client_height() {
            SCROLLBAR_WIDTH
        } else {
            0.0
        };
        let revealed = minimum_scrolling(sl, sl + client_w - overlaid, box_start, box_end);
        let new_sl = revealed.min((content_w.get() - client_w).max(0.0)).max(0.0);
        if new_sl != sl {
            view.set_scroll_left(new_sl as f64);
        }
    }
}
