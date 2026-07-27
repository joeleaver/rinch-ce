//! Drag auto-scroll: the viewport keeps scrolling while a selection drag
//! rests past its top or bottom edge — Monaco's `TopBottomDragScrolling`
//! (dragScrolling.ts), ported speed curve and all.
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
//! unmount cleanup, whichever comes first. Left/right auto-scroll arrives
//! with horizontal virtualization.
//!
//! Known gap, shared with the drag itself: the viewport origin is captured at
//! the press (rinch's capture reports raw viewport coordinates and no live
//! element bounds), so a window resize *mid-drag* skews the edge band until
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

/// Monaco's `_getScrollSpeed`, verbatim: lines per second as a three-tier
/// function of how far past the edge the pointer rests (in line heights),
/// scaled by the viewport size (in lines).
fn scroll_speed_lines(viewport_lines: f32, outside_lines: f32) -> f32 {
    if outside_lines <= 1.5 {
        (viewport_lines * (1.0 + outside_lines)).max(30.0)
    } else if outside_lines <= 3.0 {
        (viewport_lines * (2.0 + outside_lines)).max(60.0)
    } else {
        (viewport_lines * (7.0 + outside_lines)).max(200.0)
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
    origin_x: f32,
    view_top: f32,
    state: RefCell<Option<Active>>,
}

impl AutoScroll {
    pub(crate) fn new(ctx: InputCtx, mode: Rc<DragMode>, origin_x: f32, view_top: f32) -> Rc<Self> {
        Rc::new(Self {
            ctx,
            mode,
            origin_x,
            view_top,
            state: RefCell::new(None),
        })
    }

    /// A pointer position past the edge: remember it and make sure the timer
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

        let viewport = self.ctx.view.client_height() as f32;
        let above = self.view_top - py;
        let below = py - (self.view_top + viewport);
        let (dist, dir) = if viewport > 0.0 && above > 0.0 {
            (above, -1.0)
        } else if viewport > 0.0 && below > 0.0 {
            (below, 1.0)
        } else {
            // Back inside (on_move normally stops us first) or degenerate
            // geometry: go quiet.
            self.stop();
            return;
        };

        // Scroll by speed × elapsed, clamped into the document. All signal
        // writes happen after the model borrow ends — a signal write flushes
        // render effects, and those borrow the model.
        let speed = scroll_speed_lines(viewport / LINE_HEIGHT, dist / LINE_HEIGHT);
        let delta = speed * elapsed * LINE_HEIGHT * dir;
        let st = self.ctx.scroll_top.get() as f32;
        let (new_st, moved) = {
            let mut m = self.ctx.model.borrow_mut();
            let content = m.len_lines() as f32 * LINE_HEIGHT;
            let max_scroll = (content - viewport).max(0.0);
            let new_st = (st + delta).clamp(0.0, max_scroll);

            // Where the selection extends this tick — Monaco's edge targets:
            // scrolling up always lands at column 1 of the top line (its probe
            // is pinned to the bottom edge, so the above case always takes the
            // fallback); scrolling down hit-tests the pointer's x on the
            // bottom edge line, except pinned at the very bottom, where the
            // fallback takes the last line's end.
            let offset = if dir < 0.0 {
                let line = self.ctx.grid.line_at_y(new_st, m.len_lines());
                m.buffer().line_start(line)
            } else if new_st >= max_scroll {
                let line = self
                    .ctx
                    .grid
                    .line_at_y(new_st + viewport - 0.1, m.len_lines());
                m.buffer().line_end(line)
            } else {
                hit_offset(
                    &m,
                    &self.ctx.grid,
                    self.ctx.cell_w.get(),
                    px - self.origin_x,
                    new_st + viewport - 0.1,
                )
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
            (new_st, moved)
        };
        if new_st != st {
            self.ctx.view.set_scroll_top(new_st as f64);
            self.ctx.scroll_top.set(new_st as f64);
        }
        // The version bump repaints what the selection change moved; a
        // scroll-only tick needs none — the rows and gutter re-derive from
        // `scroll_top` alone, and skipping it halves the subscriber flushes.
        if moved {
            self.ctx.version.update(|v| *v = v.wrapping_add(1));
        }
        if moved || new_st != st {
            (self.ctx.blink_restart)();
        }
        self.arm();
    }
}
