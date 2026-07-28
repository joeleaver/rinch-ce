//! Drag auto-scroll: the viewport keeps scrolling while a selection drag
//! rests past one of its edges — Monaco's `TopBottomDragScrollingOperation`
//! and `LeftRightDragScrollingOperation` (`dragScrolling.ts`), ported speed
//! curve and all.
//!
//! Monaco drives its loop from animation frames; rinch has no frame
//! callback, so a short re-armed `set_timeout` stands in. The scrolling is
//! time-based (`speed × elapsed`), so the period only bounds smoothness,
//! never speed. The timer callback is one of the two signal-write-safe
//! contexts (with event handlers), but the render effects a write flushes
//! borrow the model — so every signal write here happens strictly after the
//! model borrow ends.
//!
//! One [`AutoScroll`] exists per drag: `on_move` feeds it pointer positions —
//! starting it when the pointer leaves the viewport, stopping it on re-entry
//! — and it stops for good with the drag: `on_end`, a tick after a lost drag
//! (`Drag::cancel` fires no `on_end`), the next press, or the component's
//! unmount cleanup, whichever comes first.
//!
//! # One axis at a time, and vertical wins
//!
//! There is no diagonal auto-scroll, in Monaco or here. Monaco's
//! `_getPositionOutsideEditor` tests the y first and returns before it ever
//! looks at the x, and `_onMouseDownThenMove` starts one scroller while
//! explicitly stopping the other. A pointer parked past the bottom-right
//! corner therefore scrolls straight down, not diagonally. [`Axis`] makes that
//! a single decision per tick rather than two competing timers — which also
//! keeps us clear of rinch's single global drag slot.
//!
//! # A horizontal tick freezes the line
//!
//! Scrolling sideways moves the selection's *column* and leaves its line where
//! the pointer last put it. That falls out of the geometry rather than being
//! imposed: the pointer is stationary, so re-deriving the offset against the
//! new horizontal offset advances the column and leaves the y untouched.
//! Monaco arrives at the same place by re-reading
//! `mouseContentHorizontalOffset = scrollLeft + relativeX - contentLeft` each
//! frame.
//!
//! Known gap, shared with the drag itself: the viewport origin is captured at
//! the press (rinch's capture reports raw viewport coordinates and no live
//! element bounds), so a window resize *mid-drag* skews the edge bands until
//! the next press. Monaco re-reads the editor's position per event.

use std::cell::RefCell;
use std::rc::Rc;

use rinch::core::{Drag, TimeoutHandle, clear_timeout, set_timeout};
use rinch_code_editor_model::Selections;
use web_time::Instant;

use crate::input::InputCtx;
use crate::mouse::{DragMode, GestureStart, hit_offset};
use crate::styles::LINE_HEIGHT;

/// The tick period. See the module docs for why a timer and not a frame.
const AUTO_SCROLL_TICK_MS: u32 = 50;

/// Which edge the pointer is past, and therefore which way this tick scrolls.
/// At most one is ever chosen — see the module docs.
#[derive(Clone, Copy)]
enum Axis {
    Above,
    Below,
    Left,
    Right,
}

/// Monaco's `_getScrollSpeed`, verbatim: units per second as a three-tier
/// function of how far past the edge the pointer rests, scaled by the viewport
/// size. Both axes share it — the function never cared what a "unit" was, and
/// Monaco's two operations differ only in what they pass.
///
/// The vertical caller's unit is a line. The horizontal caller's is Monaco's
/// `typicalFullwidthCharacterWidth` — the advance of `'ｍ'`, i.e. two cells of
/// a monospace grid — not one character, which is why its speeds are not
/// simply the vertical ones with a different constant.
fn scroll_speed(viewport_units: f32, outside_units: f32) -> f32 {
    if outside_units <= 1.5 {
        (viewport_units * (1.0 + outside_units)).max(30.0)
    } else if outside_units <= 3.0 {
        (viewport_units * (2.0 + outside_units)).max(60.0)
    } else {
        (viewport_units * (7.0 + outside_units)).max(200.0)
    }
}

/// A running auto-scroll: the pointer's last position and the re-armed timer.
struct Active {
    pointer_x: f32,
    pointer_y: f32,
    last_tick: Instant,
    handle: Option<TimeoutHandle>,
}

/// The per-drag auto-scroller. See the module docs.
pub(crate) struct AutoScroll {
    ctx: InputCtx,
    mode: Rc<DragMode>,
    /// The text's left edge in viewport coordinates, plus the live horizontal
    /// scroll offset — i.e. where column 0 sits when nothing is scrolled. Add
    /// the live offset back to turn a pointer x into a document x, exactly as
    /// `view_top` is used for the y.
    origin_x: f32,
    /// The scroll container's left and top edges in viewport coordinates. These
    /// are the auto-scroll boundaries: hard edges, no band, so the boundary
    /// pixel itself scrolls at zero speed. Monaco's are `contentLeft` and the
    /// minimap/scrollbar edge; ours differ only in that our gutter sits outside
    /// the scroll container, so the container's own left edge is `contentLeft`.
    view_left: f32,
    view_top: f32,
    state: RefCell<Option<Active>>,
}

impl AutoScroll {
    pub(crate) fn new(
        ctx: InputCtx,
        mode: Rc<DragMode>,
        origin_x: f32,
        view_left: f32,
        view_top: f32,
    ) -> Rc<Self> {
        Rc::new(Self {
            ctx,
            mode,
            origin_x,
            view_left,
            view_top,
            state: RefCell::new(None),
        })
    }

    /// A pointer position past an edge: remember it and make sure the timer
    /// runs. Starting `last_tick` at now means the first tick scrolls one
    /// period's worth, not the time since the drag began.
    pub(crate) fn update_pointer(self: &Rc<Self>, x: f32, y: f32) {
        let mut state = self.state.borrow_mut();
        match &mut *state {
            Some(active) => {
                active.pointer_x = x;
                active.pointer_y = y;
            }
            None => {
                *state = Some(Active {
                    pointer_x: x,
                    pointer_y: y,
                    last_tick: Instant::now(),
                    handle: None,
                });
                drop(state);
                self.arm();
            }
        }
    }

    pub(crate) fn stop(&self) {
        if let Some(active) = self.state.borrow_mut().take()
            && let Some(h) = active.handle
        {
            clear_timeout(h);
        }
    }

    fn arm(self: &Rc<Self>) {
        let me = self.clone();
        let h = set_timeout(AUTO_SCROLL_TICK_MS, move || me.tick());
        if let Some(active) = &mut *self.state.borrow_mut() {
            active.handle = Some(h);
        } else {
            // Stopped between arming and now — don't leak the timeout.
            clear_timeout(h);
        }
    }

    /// Which edge the pointer is past, and by how far. Vertical is tested
    /// first and returns before the horizontal is considered, which is what
    /// makes a corner scroll straight down rather than diagonally.
    fn axis(&self, px: f32, py: f32, viewport_w: f32, viewport_h: f32) -> Option<(Axis, f32)> {
        if viewport_h > 0.0 {
            let above = self.view_top - py;
            if above > 0.0 {
                return Some((Axis::Above, above));
            }
            let below = py - (self.view_top + viewport_h);
            if below > 0.0 {
                return Some((Axis::Below, below));
            }
        }
        if viewport_w > 0.0 {
            let left = self.view_left - px;
            if left > 0.0 {
                return Some((Axis::Left, left));
            }
            let right = px - (self.view_left + viewport_w);
            if right > 0.0 {
                return Some((Axis::Right, right));
            }
        }
        None
    }

    fn tick(self: &Rc<Self>) {
        let Some((px, py, elapsed)) = ({
            let mut state = self.state.borrow_mut();
            state.as_mut().map(|active| {
                active.handle = None;
                let now = Instant::now();
                let elapsed = now.duration_since(active.last_tick).as_secs_f32();
                active.last_tick = now;
                (active.pointer_x, active.pointer_y, elapsed)
            })
        }) else {
            return;
        };

        // The drag can die without `on_end`: rinch's `Drag::cancel()` (the
        // web backend's pointercancel path) drops the callback unfired. A
        // tick outliving its drag must not scroll a ghost.
        if !Drag::is_active() {
            self.stop();
            return;
        }

        let viewport_h = self.ctx.view.client_height() as f32;
        let viewport_w = self.ctx.view.client_width() as f32;
        let Some((axis, dist)) = self.axis(px, py, viewport_w, viewport_h) else {
            // Back inside (on_move normally stops us first) or degenerate
            // geometry: go quiet.
            self.stop();
            return;
        };

        let st = self.ctx.scroll_top.get() as f32;
        let sl = self.ctx.view.scroll_left() as f32;
        // Scroll by speed × elapsed, clamped into the document. All signal
        // writes happen after the model borrow ends — a signal write flushes
        // render effects, and those borrow the model.
        let (new_st, new_sl, moved) = {
            let mut m = self.ctx.model.borrow_mut();
            let cell_w = self.ctx.cell_w.get();
            let (mut new_st, mut new_sl) = (st, sl);

            let offset = match axis {
                Axis::Above | Axis::Below => {
                    let dir = if matches!(axis, Axis::Above) {
                        -1.0
                    } else {
                        1.0
                    };
                    let speed = scroll_speed(viewport_h / LINE_HEIGHT, dist / LINE_HEIGHT);
                    let content = m.len_lines() as f32 * LINE_HEIGHT;
                    let max_scroll = (content - viewport_h).max(0.0);
                    new_st = (st + speed * elapsed * LINE_HEIGHT * dir).clamp(0.0, max_scroll);

                    // Where the selection extends this tick — Monaco's edge
                    // targets: scrolling up always lands at column 1 of the top
                    // line (its probe is pinned to the bottom edge, so the
                    // above case always takes the fallback); scrolling down
                    // hit-tests the pointer's x on the bottom edge line, except
                    // pinned at the very bottom, where the fallback takes the
                    // last line's end.
                    if dir < 0.0 {
                        let line = self.ctx.grid.line_at_y(new_st, m.len_lines());
                        m.buffer().line_start(line)
                    } else if new_st >= max_scroll {
                        let line = self
                            .ctx
                            .grid
                            .line_at_y(new_st + viewport_h - 0.1, m.len_lines());
                        m.buffer().line_end(line)
                    } else {
                        hit_offset(
                            &m,
                            &self.ctx.grid,
                            cell_w,
                            px - self.origin_x + new_sl,
                            new_st + viewport_h - 0.1,
                        )
                    }
                }
                Axis::Left | Axis::Right => {
                    let dir = if matches!(axis, Axis::Left) {
                        -1.0
                    } else {
                        1.0
                    };
                    // Monaco measures horizontal overshoot in
                    // `typicalFullwidthCharacterWidth` — two cells — and then
                    // halves the pixel step it computes from it. Both halves
                    // are reproduced rather than cancelled into
                    // `speed * elapsed * cell_w`, because the reduction is only
                    // valid while the unit stays fullwidth, and a later reader
                    // changing one without the other would silently double the
                    // speed.
                    let fullwidth = 2.0 * cell_w;
                    let speed = scroll_speed(viewport_w / fullwidth, dist / fullwidth);
                    let max_scroll = (self.ctx.content_w.get() - viewport_w).max(0.0);
                    new_sl = (sl + speed * elapsed * fullwidth * 0.5 * dir).clamp(0.0, max_scroll);

                    // The line is frozen and only the column advances: the
                    // pointer has not moved, so re-deriving against the new
                    // horizontal offset slides the column and leaves the y
                    // exactly where the drag left it.
                    hit_offset(
                        &m,
                        &self.ctx.grid,
                        cell_w,
                        px - self.origin_x + new_sl,
                        py - self.view_top + new_st,
                    )
                }
            };

            let primary = *m.selections().primary();
            let sel = self.mode.extend(&m, offset);
            let moved = sel.anchor != primary.anchor || sel.head != primary.head;
            if moved {
                m.set_selections(Selections::single(sel));
                *self.ctx.gesture.borrow_mut() = Some(GestureStart {
                    range: self.mode.range().clone(),
                    primary: (sel.anchor, sel.head),
                });
            }
            (new_st, new_sl, moved)
        };

        if new_st != st {
            self.ctx.view.set_scroll_top(new_st as f64);
            self.ctx.scroll_top.set(new_st as f64);
        }
        // No mirrored signal on this axis, and none wanted — see [`mod@crate::scroll`].
        if new_sl != sl {
            self.ctx.view.set_scroll_left(new_sl as f64);
        }
        // The version bump repaints what the selection change moved; a
        // scroll-only tick needs none — the rows and gutter re-derive from
        // `scroll_top` alone, and skipping it halves the subscriber flushes.
        if moved {
            self.ctx.bump();
        }
        // Deliberately `blink_restart` and not `after_op`: a reveal here would
        // fight the scrolling that just happened. Monaco dispatches its
        // auto-scroll selections with `NavigationCommandRevealType.None` for
        // the same reason.
        if moved || new_st != st || new_sl != sl {
            (self.ctx.blink_restart)();
        }
        self.arm();
    }
}
