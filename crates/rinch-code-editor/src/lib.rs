//! A Monaco-parity code editor component for the rinch GUI framework.
//!
//! rinch is a *peer* dependency, not something this crate bundles — see the
//! crate README for why that is a diamond and not a cycle. Everything hard lives
//! in the three rinch-free crates below this one; here we only turn the
//! [`EditorModel`] into DOM and drive it from the keyboard.
//!
//! # Status: Increment 4d — scrolling and virtualization
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
//!
//! Still to come: the polish increment (caret blink, word/line select on
//! double/triple press, timer-driven drag auto-scroll, the resize clamp gap).

mod highlight;
mod input;
mod render;
mod styles;

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use rinch::prelude::*;
use rinch_code_editor_model::{EditorModel, Grid};

use styles::{
    EDITOR_STYLE, FALLBACK_CELL_WIDTH, LINE_HEIGHT, PROBE_LEN, PROBE_STYLE, TAB_SIZE, VIEW_STYLE,
    caret_style, current_line_style, gutter_scroll_style, gutter_style, layers_style, line_style,
    number_style, sel_rect_style,
};

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
#[component]
pub fn CodeEditor(text: String, language: String) -> NodeHandle {
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

    // The scroll container is built by hand (not via rsx) because the render needs
    // a handle to it — to measure its height for the visible-line window and to
    // scroll the caret into view. Its `onscroll` feeds `scroll_top`. The layer
    // stack is appended into it below.
    let view = __scope.create_element("div");
    view.set_attribute("style", VIEW_STYLE);
    let scroll_handler = __scope.register_scroll_handler(move |st| scroll_top.set(st));
    view.set_attribute("data-onscroll", &scroll_handler.0.to_string());

    // Reveal-and-clamp, run by the interceptor after every state-changing key.
    // Two jobs in one adjustment: bring the caret's line back into the viewport
    // when a move or edit pushed it out, and clamp the scroll when the document
    // shrank (undo, delete) — rinch's layout clamps the DOM offset silently in
    // that case (the clamp runs inside layout resolution, where firing `onscroll`
    // is unsafe), which would strand `scroll_top` and the gutter band translated
    // by it at the stale value. Mirroring the clamp here keeps signal and DOM in
    // step: the viewport height does not change on edits, so recomputing rinch's
    // own max-scroll is exact.
    //
    // This is a plain callback from input-handler context, NOT an effect, and
    // that is load-bearing: it writes `scroll_top`, a rinch signal write flushes
    // its subscribers synchronously, and a flush from inside a running effect
    // re-enters it — "RefCell already borrowed". Interceptor and mouse-handler
    // context is verified safe for flushing writes (it is where `version.update`
    // already runs); a window resize can still clamp silently, a known gap shared
    // with the synchronous viewport measurement below, left for the polish
    // increment. `Rc` because both the keyboard interceptor and the mousedown
    // handler run it after their ops.
    let reveal_and_clamp: Rc<dyn Fn()> = Rc::new({
        let vh = view.clone();
        let m = model.clone();
        move || {
            let viewport = vh.client_height() as f32;
            if viewport <= 0.0 {
                return;
            }
            let (line, content) = {
                let mb = m.borrow();
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
                vh.set_scroll_top(new_st as f64);
                scroll_top.set(new_st as f64);
            }
        }
    });

    // A hidden probe of known length: its measured width divided by its length is
    // the advance of one monospace cell.
    let probe = __scope.create_element("span");
    probe.set_attribute("style", PROBE_STYLE);
    let probe_text = __scope.create_text(&"0".repeat(PROBE_LEN));
    probe.append_child(&probe_text);

    // Derived overlay state, as memos. Each captures the model (or the probe)
    // *once* and recomputes when `version` bumps; the style closures below then
    // read them — a `Memo` is `Copy`, so nothing non-`Copy` is moved inside an
    // effect the macro re-evaluates on every run (a plain model or node handle
    // would not survive that). The `for` loops, by contrast, capture the model
    // once and only borrow it, so they reference `model` directly and the macro
    // clones it per loop.
    //
    // `cell_w` measures the probe *synchronously* (`client_width`) rather than
    // subscribing to its `bounds_signal`. That is deliberate and load-bearing: a
    // bounds-signal subscriber re-runs during paint, inside the document's read
    // borrow (see rinch's `update_bounds_signals`), and setting a style attribute
    // there re-enters the borrow and panics. Keying the measurement on `version`
    // instead means it only re-runs from the keyboard path, where the document is
    // free — and the probe's width does not depend on the document anyway, so
    // measuring it on edits (not on every layout) loses nothing. Before the first
    // layout `client_width` is 0 and the fallback stands in; the caret sits at
    // column 0 then, where the cell width does not matter.
    let cell_w = Memo::new({
        let p = probe.clone();
        move || {
            version.get();
            let w = p.client_width() as f32;
            if w > 0.0 {
                w / PROBE_LEN as f32
            } else {
                FALLBACK_CELL_WIDTH
            }
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

    // The visible line window: the slice of lines the rows below materialize.
    // Recomputes on edit (`version`) and on scroll (`scroll_top`), measuring the
    // viewport height synchronously — the same reason `cell_w` does, and the same
    // pattern rinch's own `virtual_list` uses. Before the first layout the height
    // is 0, so the fallback governs the first frame only.
    let visible = Memo::new({
        let m = model.clone();
        let vh = view.clone();
        move || {
            version.get();
            let st = scroll_top.get() as f32;
            let total = m.borrow().len_lines();
            let measured = vh.client_height() as f32;
            let viewport = if measured > 1.0 {
                measured
            } else {
                FALLBACK_VIEWPORT_H
            };
            grid.visible_lines(st, viewport, total, OVERSCAN)
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
        model: model.clone(),
        grid,
        version,
        focused,
        cell_w,
        scroll_top,
        view: view.clone(),
        after_op: reveal_and_clamp,
    };
    input::install(input_ctx.clone());
    let on_text_mousedown = input::text_mousedown(input_ctx);

    // The layer stack, sized to the whole document (the scroll spacer). Children
    // are z-ordered: current-line band, selection, text, caret. Only the visible
    // slice of rows is materialized; the caret and current-line band are single
    // elements positioned in document coordinates, so native scroll places them.
    let layers = rsx! {
        div {
            style: { move || layers_style(content_h.get()) },
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

            // Text rows (z 2) — visible slice only.
            for line in text_rows(visible, &model, &default_color) {
                div {
                    key: line.number,
                    style: line_style(line.top),
                    for seg in line.segs.clone() {
                        span {
                            key: seg.key,
                            style: format!("color:{}", seg.color),
                            { seg.text }
                        }
                    }
                }
            }

            // Caret (z 3) — in front of the text.
            div {
                style: {
                    move || {
                        let (line, vcol) = caret.get();
                        caret_style(line, vcol, cell_w.get())
                    }
                },
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
            onmousedown: move || focused.set(true),

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

// ---- reactive reads --------------------------------------------------------
//
// Each of these reads the `visible` memo — establishing the reactive dependency
// on both the document version and the scroll position it folds in — then returns
// freshly-computed render data for that window. They are named functions rather
// than inline blocks because an rsx `for`'s iterator is parsed only up to the
// first `{`, so its expression cannot itself be a block; a call is unambiguous.

fn gutter_rows(
    visible: Memo<Range<usize>>,
    model: &Rc<RefCell<EditorModel>>,
) -> Vec<render::GutterRow> {
    let range = visible.get();
    render::compute_gutter(range, model.borrow().len_lines())
}

fn selection_rects(
    visible: Memo<Range<usize>>,
    model: &Rc<RefCell<EditorModel>>,
    grid: Grid,
) -> Vec<render::SelRect> {
    let range = visible.get();
    render::compute_sel_rects(&model.borrow(), &grid, range)
}

fn text_rows(
    visible: Memo<Range<usize>>,
    model: &Rc<RefCell<EditorModel>>,
    default_color: &str,
) -> Vec<render::LineRender> {
    let range = visible.get();
    render::compute_lines(&mut model.borrow_mut(), range, default_color)
}
