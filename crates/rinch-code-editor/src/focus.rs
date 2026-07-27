//! Which editor has the keyboard, when there is more than one on screen.
//!
//! rinch routes keys through **one** global interceptor
//! ([`set_keyboard_interceptor`]) — there is no per-element `onkeydown`, and
//! installing a second interceptor replaces the first. A component that
//! installed its own would therefore work perfectly right up until somebody
//! mounted two of them, at which point the older editor would go quietly deaf.
//! So the interceptor is installed **once per process**, by whichever editor
//! mounts first, and every editor registers itself here instead.
//!
//! # Focus is ours to arbitrate
//!
//! rinch has no focus model to borrow — no focused-node registry, no blur
//! event (joeleaver/rinch#147) — so exclusive focus between editors is
//! something this module implements rather than observes. [`focus`] grants it
//! to one editor and takes it from all the others, which is what makes the
//! second editor's caret stop blinking when you click into the first, and is
//! also the only *blur* signal the editors get: an editor that loses focus is
//! told, so a collaborative embedder can withdraw its caret from the shared
//! document.
//!
//! **Clicking outside every editor** does not blur on its own, and cannot: rinch
//! delivers a mousedown to the *nearest* ancestor carrying the attribute and
//! stops, so there is no document-level event to hang it on. That same dispatch
//! rule is what makes the remedy work, though — an `onmousedown` on the
//! application's own outermost container fires only for presses that missed
//! every editor, because an in-editor press is claimed by the editor's handler
//! first. So an embedder that has other keyboard widgets on screen calls
//! [`crate::blur_all`] from there. An application that is nothing but editors
//! does not have to care, which is why this is not done for them.

use std::cell::{Cell, RefCell};

use rinch::core::{KeyEventData, set_keyboard_interceptor};

use crate::input::{self, InputCtx};

/// Identifies one mounted editor, for as long as it stays mounted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct EditorId(u64);

impl EditorId {
    /// Mint the next id. Taken *before* the editor's context is built, because
    /// the context needs to know its own id in order to claim focus.
    pub(crate) fn next() -> Self {
        NEXT_ID.with(|n| {
            let id = n.get();
            n.set(id + 1);
            Self(id)
        })
    }
}

thread_local! {
    static EDITORS: RefCell<Vec<(EditorId, InputCtx)>> = const { RefCell::new(Vec::new()) };
    static NEXT_ID: Cell<u64> = const { Cell::new(0) };
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// Add an editor to the registry, installing the process-wide key interceptor
/// if this is the first one.
pub(crate) fn register(id: EditorId, ctx: InputCtx) {
    EDITORS.with_borrow_mut(|editors| editors.push((id, ctx)));
    INSTALLED.with(|installed| {
        if !installed.get() {
            installed.set(true);
            set_keyboard_interceptor(dispatch);
        }
    });
}

/// Remove an unmounted editor. Its cleanup hook calls this: the registry holds
/// the model and the view handle, so a forgotten entry would both leak the
/// document and let keystrokes reach an editor that is no longer on screen.
///
/// Removal is by id rather than by position, and that is deliberate: rinch tears
/// sibling subtrees down in *reverse* registration order, so nothing here may
/// assume the editor being removed is the most recent one.
///
/// **Caveat worth knowing.** A component whose props are all non-closures takes
/// rinch's static path and gets no `RenderScope` of its own — its `on_cleanup`
/// lands on the *parent's* scope, so it runs when the parent is torn down rather
/// than when this one editor is removed. Removing a single editor from a live
/// tree therefore only really deregisters when it is mounted under a `Show`,
/// a `for`, or a reactive prop, each of which creates a child scope. Being
/// order-independent and idempotent is what keeps that merely coarse instead of
/// wrong.
///
/// The interceptor itself stays installed. rinch's only removal API is a global
/// clear, which would wipe an interceptor a sibling installed afterwards — and
/// with an empty registry ours is already a no-op that returns `false` and lets
/// every key through.
pub(crate) fn deregister(id: EditorId) {
    EDITORS.with_borrow_mut(|editors| editors.retain(|(other, _)| *other != id));
}

/// Give `id` the keyboard and take it from everyone else.
///
/// Every editor whose focus actually changed is notified, in both directions —
/// gaining focus and losing it. The losing half is the point: it is the only
/// blur an editor gets, and a collaborative embedder needs it to stop
/// publishing a caret it no longer owns.
pub(crate) fn focus(id: EditorId) {
    // Copy the registry out before touching any signal. A signal write flushes
    // its subscribers synchronously, so anything that runs during the write
    // would find this `RefCell` still borrowed — and one of the subscribers is
    // an editor's own render.
    for (other, ctx) in snapshot() {
        let wanted = other == id;
        if ctx.focused.get() == wanted {
            continue;
        }
        if !wanted {
            // Losing focus ends any drag this editor still thinks it is in.
            // rinch's active drag is a single global slot that `Drag::start`
            // overwrites *without* firing the previous drag's `on_end` or
            // `on_cancel`, so a press in a sibling editor silently orphans this
            // one's gesture — and its auto-scroll timer polls the same global
            // `is_active()`, which the sibling's brand-new drag has just made
            // true again. Left alone, this editor would keep scrolling itself
            // while somebody selects text in the other pane.
            if let Some(scroller) = ctx.scroller.borrow_mut().take() {
                scroller.stop();
            }
        }
        ctx.focused.set(wanted);
        ctx.notify();
    }
}

/// Take the keyboard away from every editor.
///
/// For an application that has other keyboard widgets on screen: wire this to
/// an `onmousedown` on the container that wraps everything, where rinch's
/// nearest-ancestor dispatch means it fires only for presses that missed every
/// editor. Each editor then hides its caret and, if it is in a collaborative
/// session, withdraws it from everyone else's screen too.
pub fn blur_all() {
    for (_, ctx) in snapshot() {
        if ctx.focused.get() {
            ctx.focused.set(false);
            ctx.notify();
        }
    }
}

/// The one interceptor, shared by every editor: deliver the key to whichever
/// one currently holds focus, and to nobody if none does.
fn dispatch(ev: &KeyEventData) -> bool {
    let Some(ctx) = snapshot()
        .into_iter()
        .find(|(_, ctx)| ctx.focused.get())
        .map(|(_, ctx)| ctx)
    else {
        return false;
    };
    input::handle_key(&ctx, ev)
}

/// The registry as a plain list, with the borrow released.
///
/// Everything here works from a copy rather than under the borrow, because what
/// happens next — a signal write, a whole key being handled — can reach
/// arbitrary application code, and that code is entitled to mount or unmount an
/// editor.
fn snapshot() -> Vec<(EditorId, InputCtx)> {
    EDITORS.with_borrow(|editors| editors.clone())
}
