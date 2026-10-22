//! A Monaco-parity code editor component for the rinch GUI framework.
//!
//! rinch is a *peer* dependency, not something this crate bundles — see the
//! crate README for why that is a diamond and not a cycle. Everything hard lives
//! in the three rinch-free crates below this one; here we only turn the
//! [`EditorModel`] into DOM and drive it from the keyboard.
//!
//! # Status: Increment 5 — polish (clipboard landed)
//!
//! The editor scrolls, and only materializes what is on screen. On top of the
//! live editing of 4c:
//!
//! - **Live state.** The document is an `Rc<RefCell<EditorModel>>` behind a
//!   `version` [`Signal`] that bumps on every change. Every row and overlay is
//!   produced inside a reactive closure that reads that signal, so one keystroke
//!   repaints exactly what moved and nothing else — the render is a pure function
//!   of the version, and rinch's keyed reconciliation does the diffing.
//! - **Keyboard.** A single global interceptor (rinch has no per-element key
//!   handler) drives the model — arrows and their word/line/document variants,
//!   Page keys, selection with Shift, insert/delete, Enter, Tab, and undo/redo —
//!   gated on a focus flag the editor sets when clicked. See [`mod@input`].
//! - **Real highlighting.** When a grammar is compiled in (a `lang-*` feature),
//!   the `language` prop selects a tree-sitter backend; otherwise the editor
//!   falls back to plain text. See [`mod@highlight`].
//! - **Virtualized scrolling.** The text scrolls natively inside an
//!   `overflow:auto` container whose `onscroll` feeds a `scroll_top` signal; a
//!   `visible` memo folds that with the version into the window of lines the
//!   rows materialize (plus [`OVERSCAN`]). The gutter sits outside the scroll
//!   container and follows by `translateY`. Caret moves and edits reveal the
//!   caret and clamp the scroll from the interceptor path — see the
//!   `reveal_and_clamp` comment for why that must not be an effect.
//!
//! - **Mouse selection.** A press in the text places the caret at the nearest
//!   character boundary (Monaco's midpoint rule); Shift+press extends the
//!   primary selection; dragging extends it continuously under pointer capture,
//!   revealing as it crosses the viewport edge. See [`mod@input`].
//! - **Clipboard.** Ctrl+X/C/V with Monaco's semantics — empty-selection copy
//!   and cut take the whole line, pasting a whole-line copy inserts it above
//!   the caret's line, multi-cursor copies distribute back piece-per-cursor —
//!   through `rinch::clipboard` plus Monaco's in-memory metadata store. See
//!   [`mod@clipboard`] (and its web caveat).
//! - **Collaborators.** Given a `peers` signal, the editor draws everyone else's
//!   selection, caret and name — tinted behind the glyphs, the caret solid in
//!   front, the name chip up for a moment after they move. Where the presence
//!   comes from is not this crate's business: it takes plain
//!   [`RemotePresence`]es and never mentions a network. See [`mod@presence`].
//! - **Being** a collaborator. An [`EditorHandle`] passed as the `handle` prop
//!   lets an embedder replay a peer's edits into this document and collect this
//!   editor's own for publication, and `on_change` tells it when to. The
//!   collaboration crate is not a dependency of this one and never will be —
//!   the application is the only place that names both. See [`mod@handle`].
//!
//! # More than one editor
//!
//! Several `CodeEditor`s can share a window. rinch routes keys through one
//! process-wide interceptor, so the editors keep a registry and arbitrate the
//! keyboard among themselves; clicking one takes focus from the others, and an
//! editor that loses focus withdraws its caret from any collaborative session.
//! [`mod@focus`] documents the mechanism and, more importantly, the three things
//! it still cannot do: blur when a click lands outside every editor (wire
//! [`blur_all`] if that matters to you), blur when the OS window loses focus,
//! and let a rinch `<input>` receive keys while an editor is focused.
//!
//! Two further constraints worth stating rather than discovering:
//!
//! - **One [`EditorHandle`] per mounted editor.** A handle is a live connection
//!   to one document, and attaching one to two editors would leave the first
//!   silently orphaned. Same rule as rinch's own editor, and as y-monaco's.
//! - **Per-instance teardown needs a control-flow boundary.** rinch gives a
//!   component its own scope — and therefore its own cleanup — only inside a
//!   `Show`, a `for`, or an `if`. Mounted statically, an editor's cleanups land
//!   on its parent's list and run when the *parent* goes, which is fine when
//!   editors live as long as the window and not fine when they come and go.

mod autoscroll;
mod blink;
mod clipboard;
mod focus;
mod handle;
mod highlight;
mod input;
mod mouse;
mod presence;
mod render;
mod scroll;
mod styles;
mod view_state;

use std::cell::RefCell;
use std::rc::Rc;

use rinch::prelude::*;
use rinch_code_editor_model::{EditorModel, Grid};

pub use focus::blur_all;
pub use handle::EditorHandle;

/// How a collaborator is described to the editor, and the types an embedder
/// needs to talk to it. Re-exported so wiring a collaborative session needs
/// only this crate on the view side — mapping its own peer type (a
/// `rinch_code_editor_collab::Peer`, or anything else) into a
/// [`RemotePresence`], and passing [`Change`]s and [`Splice`]s across through
/// an [`EditorHandle`].
pub use rinch_code_editor_model::{Change, RemotePresence, Rgba, Selection, Splice};

// Imported by name, not called through `render::`: the rsx `for` macro captures
// and clones the idents its iterator expression names, and a module path is not
// a value. See the note above their definitions.
use presence::{ChipClock, remote_carets, remote_chips, remote_dots, remote_rects};
use render::{gutter_rows, selection_rects, text_rows};
use styles::{
    EDITOR_STYLE, LINE_HEIGHT, PROBE_LEN, PROBE_STYLE, STOP_RENDERING_LINE_AFTER, TAB_SIZE,
    VIEW_STYLE, caret_style, current_line_style, gutter_scroll_style, gutter_style, layers_style,
    line_style, number_style, remote_caret_style, remote_chip_style, remote_dot_style,
    remote_sel_rect_style, sel_rect_style,
};
use view_state::ViewState;

/// Extra lines rendered above and below the viewport so a fast scroll does not
/// flash blank rows before the next frame materializes them.
const OVERSCAN: usize = 6;
/// Viewport height assumed before the first layout has measured the real one.
/// Generous on purpose: it only governs the very first frame (`client_height` is
/// 0 until layout), and over-rendering a few dozen extra rows once is free next
/// to leaving the bottom of a tall window blank until the first scroll or edit.
const FALLBACK_VIEWPORT_H: f32 = 1200.0;

/// The editor component. `text` is the initial document; `language` names a
/// bundled grammar (e.g. `"rust"`) — an empty or not-compiled-in language renders
/// as plain text.
///
/// `peers` are the collaborators to draw. It is a [`Signal`] and not a plain
/// `Vec` because a component's body runs exactly once, so anything passed by
/// value is frozen at mount; the embedder writes the signal as presence arrives
/// and the overlays follow. Leaving it out gives an empty list and an editor
/// that renders nobody, which is what a non-collaborative editor wants.
///
/// Pass the signal *itself*, never a closure producing one: a closure-valued
/// prop makes rinch treat the whole component as reactive and rebuild its entire
/// subtree on every presence update.
///
/// `on_change` fires after anything that moves the document or the selection,
/// and after this editor gains or loses the keyboard. Monaco splits this into
/// `onDidChangeModelContent` and `onDidChangeCursorSelection`; one event is
/// enough here because a collaborative embedder's response to both is the same
/// — drain the pending changes (often none), republish the caret, pump. The
/// prop name must keep its `on_` prefix: rinch treats other closure-valued
/// props as making the whole component reactive, which would rebuild its
/// subtree on every keystroke.
#[component]
pub fn CodeEditor(
    text: String,
    language: String,
    peers: Signal<Vec<RemotePresence>>,
    on_change: Callback,
    handle: Option<EditorHandle>,
) -> NodeHandle {
    // This editor's identity among all the mounted ones, taken before the input
    // context that carries it. See [`mod@focus`] for what it is for.
    let editor_id = focus::EditorId::next();

    // Build the theme, read the default foreground off it (the fallback colour for
    // an unstyled token), then hand the theme to the highlighter it colours through.
    let theme = highlight::dark_theme();
    let default_color = theme
        .color(theme.default_metadata().foreground())
        .map(|c| c.to_css())
        .unwrap_or_else(|| "#d4d4d4".to_string());
    let highlighter = highlight::make_highlighter(&language, theme);

    // The one owner of the document. `RefCell` because the keyboard interceptor
    // and the render effects both reach it; `Rc` because they outlive this call.
    let model = Rc::new(RefCell::new(EditorModel::from_text(&text, highlighter)));

    // The grid's cell width is a placeholder (1.0): visual columns are counted in
    // cells, independent of pixels, and the overlays multiply the measured cell
    // advance in reactively after layout.
    let grid = Grid::new(1.0, LINE_HEIGHT, TAB_SIZE);

    // `version` bumps on every model change; `scroll_top` tracks the viewport's
    // scroll offset. Together they drive the whole reactive render: a keystroke
    // repaints the rows and overlays that moved, a scroll re-slices the visible
    // window. `focused` gates the keyboard interceptor so keys only reach us while
    // the editor is active.
    let version = Signal::new(0u64);
    let scroll_top = Signal::new(0.0_f64);
    let focused = Signal::new(false);
    // How many characters of any one line get drawn — Monaco's
    // `stopRenderingLineAfter`. A signal rather than a constant because
    // clicking at or past the cap lifts it for this editor, which is the only
    // way to reach text a capped line does not draw. `usize::MAX` is off.
    let stop_after = Signal::new(STOP_RENDERING_LINE_AFTER);

    // The scroll container is built by hand (not via rsx) because the render needs
    // a handle to it — to measure its height for the visible-line window and to
    // scroll the caret into view. Its `onscroll` feeds `scroll_top`. The layer
    // stack is appended into it below.
    let view = __scope.create_element("div");
    view.set_attribute("style", VIEW_STYLE);
    let scroll_handler = __scope.register_scroll_handler(move |ev| scroll_top.set(ev.scroll_top));
    view.set_attribute("data-onscroll", &scroll_handler.0.to_string());

    // A hidden probe of known length: its measured width divided by its length is
    // the advance of one monospace cell.
    let probe = __scope.create_element("span");
    probe.set_attribute("style", PROBE_STYLE);
    let probe_text = __scope.create_text(&"0".repeat(PROBE_LEN));
    probe.append_child(&probe_text);

    // Everything the render derives from the document and the scroll position,
    // as memos: the measured cell advance, the document's height and width, the
    // caret, and the visible line window. Destructured into locals because the
    // rsx block below reads them by name from inside reactive closures — see
    // [`mod@view_state`] for why each is a `Memo` and why the measurements are
    // taken synchronously.
    //
    // These come before the scroll adjustment because it reads two of them: the
    // caret's pixel column needs the measured cell advance, and its horizontal
    // clamp needs the document width.
    let ViewState {
        cell_w,
        content_h,
        content_w,
        current_line,
        caret,
        visible,
    } = ViewState::new(
        &model,
        grid,
        version,
        scroll_top,
        stop_after,
        probe.clone(),
        view.clone(),
    );

    // Reveal-and-clamp, run after every state-changing operation: bring the
    // caret back into view on both axes, and mirror the clamp rinch's layout
    // applies silently when the document shrinks under the viewport. It must
    // stay a plain callback from input-handler context rather than an effect —
    // see [`mod@scroll`] for that, and for why the horizontal half deliberately
    // keeps no signal of its own.
    let reveal_and_clamp = scroll::reveal_and_clamp(
        view.clone(),
        model.clone(),
        grid,
        cell_w,
        scroll_top,
        stop_after,
        content_w,
    );

    // The caret blinker: solid through activity, pulsing when idle, hidden
    // unfocused — its tick also reconciles the silent resize clamp. Restarted
    // by `after_op` below and by the focus handlers, i.e. on exactly the
    // cursor-state changes Monaco restarts on. Stopped on unmount: the timer
    // chain re-arms itself and a parked callback owns the blinker, so without
    // the cleanup it would tick a ghost editor forever.
    let blinker = blink::Blinker::new(focused, scroll_top, view.clone());
    __scope.on_cleanup({
        let blinker = blinker.clone();
        move || blinker.stop()
    });

    // Collaborators' name chips: up for a moment after that peer moves, then
    // down again. The clock derives "that peer moved" by diffing the presence
    // list against the last one it saw, and owns the timer that takes the chips
    // down; `chip_tick` is that timer's only output, and exists purely to
    // invalidate the render. Stopped on unmount for the blinker's reason — a
    // parked timer callback holds an `Rc` of it.
    let chip_tick = Signal::new(0u64);
    let chips = ChipClock::new(chip_tick);
    __scope.on_cleanup({
        let chips = chips.clone();
        move || chips.stop()
    });

    // Everything that runs after a state-changing operation, from the safe
    // input-handler context: reveal the caret and clamp the scroll, then give
    // the caret a fresh solid half-period.
    let after_op: Rc<dyn Fn(bool)> = Rc::new({
        let blinker = blinker.clone();
        move |minimal| {
            reveal_and_clamp(minimal);
            blinker.restart();
        }
    });

    // The input surfaces, wired to one shared context: the global keyboard
    // interceptor, and mouse placement/drag selection on the layer stack. The
    // mousedown handler goes on the layer stack — document-sized and scrolling
    // with the text — so the press's element-relative position is already a
    // document position; the conversion to a line and column lives in
    // [`input::text_mousedown`]. Because rinch dispatches a mousedown to the
    // *nearest* ancestor carrying the attribute, that handler shadows the editor
    // root's focus-only one everywhere in the text area, so it takes focus too.
    let input_ctx = input::InputCtx {
        id: editor_id,
        model: model.clone(),
        grid,
        version,
        focused,
        cell_w,
        content_w,
        scroll_top,
        stop_after,
        view: view.clone(),
        after_op,
        blink_restart: Rc::new({
            let blinker = blinker.clone();
            move || blinker.restart()
        }),
        clicks: Rc::new(RefCell::new(mouse::ClickCounter::new())),
        gesture: Rc::new(RefCell::new(None)),
        scroller: Rc::new(RefCell::new(None)),
        on_change: Rc::new(move || on_change.invoke()),
    };
    // A drag's auto-scroll timer must not outlive the component either.
    __scope.on_cleanup({
        let slot = input_ctx.scroller.clone();
        move || {
            if let Some(s) = slot.borrow_mut().take() {
                s.stop();
            }
        }
    });
    // Join the registry that owns the one process-wide key interceptor, and
    // leave it on unmount — an entry left behind would keep this editor's model
    // alive and keep feeding it keystrokes after it left the screen.
    focus::register(editor_id, input_ctx.clone());
    __scope.on_cleanup(move || focus::deregister(editor_id));

    // If the embedder brought a handle, fill it in: from here it can replay a
    // collaborator's edits into this document and collect ours for publication.
    // Holding a handle is also what turns local-change recording on, so an
    // editor nobody is collaborating through pays nothing for the feature.
    if let Some(handle) = handle {
        handle.attach(model.clone(), version, focused);
        __scope.on_cleanup(move || handle.detach());
    }
    let on_text_mousedown = mouse::text_mousedown(input_ctx);

    // The caret's style closure reads these two `Copy` signals — never the
    // `Rc<Blinker>` itself, which a reactive style block must not capture.
    let blink_visible = blinker.visible;

    // The layer stack, sized to the whole document (the scroll spacer). Children
    // are z-ordered: current-line band, selection, text, caret. Only the visible
    // slice of rows is materialized; the caret and current-line band are single
    // elements positioned in document coordinates, so native scroll places them.
    let layers = rsx! {
        div {
            style: { move || layers_style(content_w.get(), content_h.get()) },
            onmousedown: on_text_mousedown,

            // Current-line band (z 0) — full width, no measurement needed.
            div {
                style: { move || current_line_style(current_line.get()) },
            }

            // Selection rectangles (z 1) — behind the glyphs, visible slice only.
            for rect in selection_rects(visible, &model, grid) {
                div {
                    key: rect.key,
                    style: {
                        move || {
                            sel_rect_style(rect.line, rect.start_vcol, rect.end_vcol, cell_w.get())
                        }
                    },
                }
            }

            // Collaborators' selections (z 1) — emitted after the local ones so
            // that at equal depth, DOM order puts a peer's tint over yours.
            for rect in remote_rects(visible, peers, &model, grid) {
                div {
                    key: format!("{}:{}", rect.peer, rect.span.line),
                    style: {
                        move || {
                            remote_sel_rect_style(
                                rect.span.line,
                                rect.span.start_vcol,
                                rect.span.end_vcol,
                                cell_w.get(),
                                rect.tint,
                            )
                        }
                    },
                }
            }

            // Text rows (z 2) — visible slice only.
            for line in text_rows(visible, &model, &default_color, stop_after) {
                div {
                    key: line.number,
                    style: line_style(line.top),
                    for seg in line.segs.clone() {
                        span {
                            key: seg.key,
                            style: seg.style.clone(),
                            { seg.text }
                        }
                    }
                }
            }

            // Caret (z 3) — in front of the text. Shown only while the editor
            // is focused and the blink is in its visible half; toggling
            // `display` at an unchanged position keeps rinch's dirty-region
            // fast path (a caret *move* is what forces the full repaint).
            div {
                style: {
                    move || {
                        let (line, vcol) = caret.get();
                        let shown = focused.get() && blink_visible.get();
                        caret_style(line, vcol, cell_w.get(), shown)
                    }
                },
            }

            // Collaborators' carets (z 3) — solid, never blinking: a blink means
            // "your next keystroke lands here", which is a claim only the local
            // caret can make.
            //
            // The `let`s live *inside* the style block, not in front of the
            // element: a reactive style is re-evaluated on every effect run, so
            // it may not move a non-`Copy` capture, and a `PresenceCaret` owns
            // its name. Lifting the `Copy` fields out inside the block satisfies
            // that (the block's setup runs each time, then returns the closure)
            // without putting bindings in front of the element, where the macro
            // would also copy them into the key closure and warn them unused.
            for c in remote_carets(visible, peers, &model, grid) {
                div {
                    key: c.peer,
                    style: {
                        let (line, vcol) = (c.line, c.vcol);
                        let (reversed, color) = (c.reversed, c.color);
                        move || remote_caret_style(line, vcol, cell_w.get(), reversed, color)
                    },
                }
            }

            // The dot capping an idle collaborator's caret (z 4), and the name
            // chip that replaces it while they are active. They are two loops
            // over two disjoint lists rather than one list with a visibility
            // flag: reconciliation reuses a node whose key has not changed, so a
            // flag read at build time would never be re-read. Moving a peer
            // between the lists is an insertion and a removal, which it handles.
            for c in remote_dots(visible, peers, chip_tick, &chips, &model, grid) {
                div {
                    key: c.peer,
                    style: {
                        let (line, vcol) = (c.line, c.vcol);
                        let (reversed, color) = (c.reversed, c.color);
                        move || remote_dot_style(line, vcol, cell_w.get(), reversed, color)
                    },
                }
            }

            for c in remote_chips(visible, peers, chip_tick, &chips, &model, grid) {
                // The one binding that has to sit out here: the chip's text is a
                // child node, and the style effect below captures `c` by move.
                let name = c.name.clone();
                div {
                    // The name is part of the key, so a rename rebuilds the chip
                    // — a text child is written once, when the node is built.
                    key: format!("{}:{}", c.peer, name),
                    style: {
                        let (line, vcol) = (c.line, c.vcol);
                        let (reversed, color) = (c.reversed, c.color);
                        let label = c.label_color;
                        move || {
                            remote_chip_style(line, vcol, cell_w.get(), reversed, color, label)
                        }
                    },
                    { name.clone() }
                }
            }

            // The measurement probe (hidden).
            { probe }
        }
    };
    view.append_child(&layers);

    rsx! {
        div {
            style: EDITOR_STYLE,
            // Focus-only fallback: reached when a press misses the layer stack —
            // the gutter, or below a document shorter than the viewport. (The
            // layer-stack handler shadows this one over the text; see above.)
            // Taking focus shows the caret, so the blink starts here too.
            onmousedown: {
                let blinker = blinker.clone();
                move || {
                    focus::focus(editor_id);
                    blinker.restart();
                }
            },

            // Gutter: a clipped column whose inner band is translated to match the
            // text's native scroll, so its numbers slide in lockstep. Only the
            // visible slice of numbers is materialized.
            div {
                style: gutter_style(),
                div {
                    style: { move || gutter_scroll_style(scroll_top.get(), content_h.get()) },
                    for row in gutter_rows(visible, &model) {
                        div {
                            key: row.number,
                            style: number_style(row.top),
                            { row.number.to_string() }
                        }
                    }
                }
            }

            { view }
        }
    }
}
