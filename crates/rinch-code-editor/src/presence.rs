//! Collaborators on screen: their selections, their carets, and their names.
//!
//! The geometry is not here — it is in the model crate's [`mod@presence`]
//! module, rinch-free and unit-tested, computed in the same grid coordinates as
//! the local selection. What is here is the part that needs a renderer: the
//! reactive reads the component's `for` loops call, and the small state machine
//! that decides when a name chip is up.
//!
//! # One caret, three loops
//!
//! A caret draws as three separate nodes — the bar, the dot capping it, and the
//! name chip — and an rsx `for` body produces exactly one node per item. So the
//! component runs three loops over the same peers rather than one loop emitting
//! three children. That is cheaper than it looks (the list is a handful of
//! entries), and it keeps every overlay a flat, absolutely-positioned child of
//! the layer stack, like every other overlay in the editor. Nesting a per-peer
//! wrapper instead would need either a zero-dimension element — the exact shape
//! rinch's paint fast path mishandles (joeleaver/rinch#142) — or a second
//! full-document-sized layer per peer.
//!
//! # Why the chip is timed and not hovered
//!
//! VS Code Live Share's default name-tag policy is `Activity`: the tag appears
//! when that participant does something and hides again 1.5 s later. We follow
//! it, rather than y-codemirror.next's show-on-hover, for one practical reason
//! and one principled one. Practically, rinch's desktop hover dispatch tests the
//! raw hit node rather than the element carrying the handler, so a pointer
//! crossing between two children of the same handler fires leave→enter and a
//! hover-driven chip flickers; hover is also suppressed outright during a drag
//! selection. Principled: a label that only appears when you go looking for it
//! cannot tell you somebody *just* typed, which is the one thing a name chip is
//! for.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use rinch::core::{Memo, Signal, TimeoutHandle, clear_timeout, set_timeout};
use rinch_code_editor_model::{
    EditorModel, Grid, PresenceCaret, PresenceRect, RemotePresence, presence_carets, presence_rects,
};
use web_time::Instant;

/// How long a name chip stays up after that peer last did something.
///
/// Live Share's dwell for `nameTagVisibility: "Activity"`.
const CHIP_MS: u64 = 1500;

/// Which collaborators are currently labelled, and the timer that stops them
/// being labelled.
///
/// Presence arrives as a whole list, with no "what changed" alongside it — the
/// collaborative crate bumps one version counter for any change at all. So
/// activity is derived here, by diffing each frame's list against the last one
/// this saw. That keeps the embedder's job to publishing facts: it never has to
/// remember to stamp a timestamp, and cannot get the policy subtly wrong.
pub(crate) struct ChipClock {
    /// Per peer: when they last changed, and what they looked like then.
    seen: RefCell<HashMap<u64, (Instant, RemotePresence)>>,
    pending: RefCell<Option<TimeoutHandle>>,
    /// The deadline `pending` was armed for. A re-render that changes no
    /// presence — a scroll, a keystroke — recomputes the same deadline, and
    /// comparing against this is what stops it cancelling and re-arming the
    /// timer on every scroll event.
    armed_for: Cell<Option<Instant>>,
    /// Bumped when a chip's time is up, purely to invalidate the render. The
    /// write happens in a timer callback, which is one of the two contexts
    /// verified safe for a signal write that flushes subscribers synchronously
    /// (see [`mod@crate::blink`]).
    tick: Signal<u64>,
    dead: Cell<bool>,
}

impl ChipClock {
    pub(crate) fn new(tick: Signal<u64>) -> Rc<Self> {
        Rc::new(Self {
            seen: RefCell::new(HashMap::new()),
            pending: RefCell::new(None),
            armed_for: Cell::new(None),
            tick,
            dead: Cell::new(false),
        })
    }

    /// Stop for good — the component's unmount hook. A parked timer callback
    /// owns an `Rc` of this and would otherwise keep waking a dead editor.
    pub(crate) fn stop(&self) {
        self.dead.set(true);
        if let Some(h) = self.pending.borrow_mut().take() {
            clear_timeout(h);
        }
        self.armed_for.set(None);
    }

    /// Fold this frame's peer list in and report who is labelled.
    ///
    /// Idempotent, which is load-bearing: all three caret loops call it within
    /// one render pass, and a second call with the same list must not re-stamp
    /// anybody (that would make a chip immortal) or re-arm the timer.
    fn refresh(self: &Rc<Self>, peers: &[RemotePresence]) -> Vec<u64> {
        let now = Instant::now();
        let dwell = Duration::from_millis(CHIP_MS);
        let mut labelled = Vec::new();
        let mut next: Option<Instant> = None;

        {
            let mut seen = self.seen.borrow_mut();
            // Forget departed peers, or their stamps accumulate for the life of
            // the editor and a peer who left and came back is never announced.
            seen.retain(|id, _| peers.iter().any(|p| p.id == *id));

            for peer in peers {
                let entry = seen.entry(peer.id).or_insert_with(|| (now, peer.clone()));
                // A peer appearing for the first time is "activity" too: opening
                // a document should tell you who is already in it.
                if entry.1 != *peer {
                    *entry = (now, peer.clone());
                }
                let deadline = entry.0 + dwell;
                if deadline > now {
                    labelled.push(peer.id);
                    next = Some(next.map_or(deadline, |d: Instant| d.min(deadline)));
                }
            }
        }

        self.arm(next, now);
        labelled
    }

    fn arm(self: &Rc<Self>, deadline: Option<Instant>, now: Instant) {
        if self.dead.get() || self.armed_for.get() == deadline {
            return;
        }
        if let Some(h) = self.pending.borrow_mut().take() {
            clear_timeout(h);
        }
        self.armed_for.set(deadline);
        let Some(deadline) = deadline else {
            return;
        };
        // At least 1ms: a zero delay would fire inside this render pass.
        let delay = (deadline.saturating_duration_since(now).as_millis() as u32).max(1);
        let me = self.clone();
        let handle = set_timeout(delay, move || me.expire());
        *self.pending.borrow_mut() = Some(handle);
    }

    fn expire(self: &Rc<Self>) {
        self.pending.borrow_mut().take();
        self.armed_for.set(None);
        if self.dead.get() {
            return;
        }
        self.tick.update(|t| *t = t.wrapping_add(1));
    }
}

// ---- reactive reads --------------------------------------------------------
//
// Same contract as the ones in [`mod@crate::render`]: read the signals first so
// the dependency is established, then compute. Called from an rsx `for`, so each
// must be a plain call — the iterator expression is parsed only up to the first
// `{` and cannot be a block.
//
// Note what is *not* here: a caret does not carry a "chip is up" flag for the
// view to branch on. A flag baked into an item at build time would go stale,
// because keyed reconciliation reuses a node whose key has not changed and never
// re-runs the code that read the flag. So the labelled and unlabelled carets are
// separated into two lists instead, and a chip going up or down is an insertion
// in one loop and a removal in the other — which reconciliation *does* handle.

/// Every collaborator's selection rectangles, for the visible window.
pub(crate) fn remote_rects(
    visible: Memo<Range<usize>>,
    peers: Signal<Vec<RemotePresence>>,
    model: &Rc<RefCell<EditorModel>>,
    grid: Grid,
) -> Vec<PresenceRect> {
    let range = visible.get();
    let peers = peers.get();
    let model = model.borrow();
    presence_rects(&peers, model.buffer(), &grid, range)
}

/// Every collaborator's caret bar in the visible window.
///
/// Deliberately does not read `chip_tick`: a chip's dwell expiring changes
/// nothing about where a caret is, and reading it here would rebuild every bar
/// twice a peer.
pub(crate) fn remote_carets(
    visible: Memo<Range<usize>>,
    peers: Signal<Vec<RemotePresence>>,
    model: &Rc<RefCell<EditorModel>>,
    grid: Grid,
) -> Vec<PresenceCaret> {
    let range = visible.get();
    let peers = peers.get();
    let model = model.borrow();
    presence_carets(&peers, model.buffer(), &grid, range)
}

/// The carets whose owner is currently labelled — these get a name chip.
pub(crate) fn remote_chips(
    visible: Memo<Range<usize>>,
    peers: Signal<Vec<RemotePresence>>,
    chip_tick: Signal<u64>,
    clock: &Rc<ChipClock>,
    model: &Rc<RefCell<EditorModel>>,
    grid: Grid,
) -> Vec<PresenceCaret> {
    labelled_carets(visible, peers, chip_tick, clock, model, grid, true)
}

/// The carets whose owner is not currently labelled — these get the plain dot.
pub(crate) fn remote_dots(
    visible: Memo<Range<usize>>,
    peers: Signal<Vec<RemotePresence>>,
    chip_tick: Signal<u64>,
    clock: &Rc<ChipClock>,
    model: &Rc<RefCell<EditorModel>>,
    grid: Grid,
) -> Vec<PresenceCaret> {
    labelled_carets(visible, peers, chip_tick, clock, model, grid, false)
}

/// The shared body of the two above. Reading `chip_tick` is what lets a chip
/// come *down* on its own once the clock says its dwell is over.
fn labelled_carets(
    visible: Memo<Range<usize>>,
    peers: Signal<Vec<RemotePresence>>,
    chip_tick: Signal<u64>,
    clock: &Rc<ChipClock>,
    model: &Rc<RefCell<EditorModel>>,
    grid: Grid,
    want_labelled: bool,
) -> Vec<PresenceCaret> {
    let range = visible.get();
    let peers = peers.get();
    chip_tick.get();
    // Both callers run this within one render pass; `refresh` is idempotent for
    // exactly that reason.
    let labelled = clock.refresh(&peers);
    let model = model.borrow();
    presence_carets(&peers, model.buffer(), &grid, range)
        .into_iter()
        .filter(|caret| labelled.contains(&caret.peer) == want_labelled)
        .collect()
}
