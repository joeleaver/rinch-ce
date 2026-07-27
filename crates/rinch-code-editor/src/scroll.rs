//! Keeping the viewport and the `scroll_top` signal telling the same story.
//!
//! The text scrolls natively — its container has `overflow:auto` and the browser
//! or the desktop layout engine moves it — while a `scroll_top` signal mirrors
//! that offset for everything laid out beside the text: the visible-line window,
//! the gutter band. Two things can put the mirror out of step with the DOM, and
//! [`reveal_and_clamp`] handles both in one adjustment.

use std::cell::RefCell;
use std::rc::Rc;

use rinch::core::{NodeHandle, Signal};
use rinch_code_editor_model::{EditorModel, Grid};

use crate::render;
use crate::styles::LINE_HEIGHT;

/// Build the after-every-operation scroll adjustment: bring the caret's line
/// back into the viewport when a move or an edit pushed it out, and clamp the
/// scroll when the document shrank.
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
    scroll_top: Signal<f64>,
) -> impl Fn() + 'static {
    move || {
        let viewport = view.client_height() as f32;
        if viewport <= 0.0 {
            return;
        }
        let (line, content) = {
            let mb = model.borrow();
            (
                render::caret_grid(&mb, &grid).0,
                render::content_height(&mb),
            )
        };
        let st = scroll_top.get() as f32;
        let top = line as f32 * LINE_HEIGHT;
        let bottom = top + LINE_HEIGHT;
        let revealed = if top < st {
            top
        } else if bottom > st + viewport {
            bottom - viewport
        } else {
            st
        };
        let new_st = revealed.min((content - viewport).max(0.0)).max(0.0);
        if new_st != st {
            view.set_scroll_top(new_st as f64);
            scroll_top.set(new_st as f64);
        }
    }
}
