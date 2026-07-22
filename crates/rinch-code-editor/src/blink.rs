//! Caret blink — Monaco's cancel-and-restart interval — and the housekeeping
//! that rides its tick.
//!
//! Monaco's `ViewCursors` blinks the caret with a 500 ms JS interval
//! (`BLINK_INTERVAL`), and *every* cursor state change — a keystroke, an
//! arrow, a click — cancels the interval, shows the caret, and starts a fresh
//! half-period. The observable effect, which this module reproduces exactly:
//! the caret is solid while you type and for a full half-period after any
//! activity, and only starts pulsing once you pause. An unfocused editor
//! shows no caret at all.
//!
//! The timer is rinch's one-shot `set_timeout`, re-armed from its own
//! callback. That context is load-bearing: a timeout callback runs as
//! top-level main-thread work with the document RefCell free — the same
//! safety class as an input handler — so it is one of the few places a signal
//! write (which flushes render effects synchronously) cannot re-enter a
//! running borrow. See the crash lessons in [`crate`]'s input module.
//!
//! # The tick's second job
//!
//! rinch's layout clamps a scroll container's DOM offset silently when the
//! viewport grows past the content (a window resize — there is no resize
//! event and `bounds_signal` does not refresh on a pure resize), which would
//! strand the `scroll_top` signal, and with it the gutter band and the
//! visible-line window, at a stale value. Every operation already re-clamps
//! (the `after_op` reveal), so the only exposure is a resize while idle; the
//! blink tick closes it by reconciling the signal with the DOM's actual
//! offset twice a second while the editor is focused. A resize while
//! *unfocused* stays stale until the next interaction — the caret is hidden
//! then and the first click reconciles, so nothing visibly wrong survives.

use std::cell::RefCell;
use std::rc::Rc;

use rinch::core::{NodeHandle, Signal, TimeoutHandle, clear_timeout, set_timeout};

/// Monaco's `ViewCursors.BLINK_INTERVAL`: the half-period, in milliseconds.
const BLINK_INTERVAL_MS: u32 = 500;

/// The blink state machine. One per editor, shared `Rc` between the input
/// callbacks that restart it and the timer callback that advances it.
pub(crate) struct Blinker {
    /// Whether the caret is in the visible half of the blink. The caret's
    /// style closure reads this (a `Signal` is `Copy`, so the closure captures
    /// it directly — the render must not capture the `Rc` itself).
    pub(crate) visible: Signal<bool>,
    focused: Signal<bool>,
    scroll_top: Signal<f64>,
    view: NodeHandle,
    handle: RefCell<Option<TimeoutHandle>>,
}

impl Blinker {
    pub(crate) fn new(
        focused: Signal<bool>,
        scroll_top: Signal<f64>,
        view: NodeHandle,
    ) -> Rc<Self> {
        Rc::new(Self {
            visible: Signal::new(true),
            focused,
            scroll_top,
            view,
            handle: RefCell::new(None),
        })
    }

    /// Monaco's `_updateBlinking`: cancel the pending toggle, show the caret,
    /// and start a fresh half-period. Called on focus and after every
    /// operation, which is exactly what keeps the caret solid under activity.
    pub(crate) fn restart(self: &Rc<Self>) {
        if let Some(h) = self.handle.borrow_mut().take() {
            clear_timeout(h);
        }
        if !self.visible.get() {
            self.visible.set(true);
        }
        if self.focused.get() {
            self.arm();
        }
    }

    fn arm(self: &Rc<Self>) {
        let me = self.clone();
        let h = set_timeout(BLINK_INTERVAL_MS, move || me.tick());
        *self.handle.borrow_mut() = Some(h);
    }

    fn tick(self: &Rc<Self>) {
        self.handle.borrow_mut().take();
        // Focus lost since arming: go quiet. The focus click restarts us.
        if !self.focused.get() {
            return;
        }
        self.visible.update(|v| *v = !*v);
        self.reconcile_scroll();
        self.arm();
    }

    /// Re-align the `scroll_top` signal with the DOM's actual offset — see
    /// the module docs for why layout can move the latter silently.
    fn reconcile_scroll(&self) {
        let dom = self.view.scroll_top();
        if self.scroll_top.get() != dom {
            self.scroll_top.set(dom);
        }
    }
}
