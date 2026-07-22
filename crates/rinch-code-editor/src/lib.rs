//! A Monaco-parity code editor component for the rinch GUI framework.
//!
//! rinch is a *peer* dependency, not something this crate bundles — see the
//! crate README for why that is a diamond and not a cycle. Everything hard lives
//! in the three rinch-free crates below this one; here we only turn the
//! [`EditorModel`] into DOM and drive it from the keyboard.
//!
//! # Status: Increment 4c — live editing
//!
//! The editor is now interactive. Three things landed on top of the read-only
//! overlays of 4b:
//!
//! - **Live state.** The document is an `Rc<RefCell<EditorModel>>` behind a
//!   `version` [`Signal`] that bumps on every change. Every row and overlay is
//!   produced inside a reactive closure that reads that signal, so one keystroke
//!   repaints exactly what moved and nothing else — the render is a pure function
//!   of the version, and rinch's keyed reconciliation does the diffing.
//! - **Keyboard.** A single global interceptor (rinch has no per-element key
//!   handler) drives the model — arrows and their word/line/document variants,
//!   selection with Shift, insert/delete, Enter, Tab, and undo/redo — gated on a
//!   focus flag the editor sets when clicked. See [`mod@input`].
//! - **Real highlighting.** When a grammar is compiled in (a `lang-*` feature),
//!   the `language` prop selects a tree-sitter backend; otherwise the editor
//!   falls back to plain text. See [`mod@highlight`].
//!
//! Still to come: click-to-place-caret hit testing, and viewport virtualization
//! (4d) so the whole-document walks in [`mod@render`] become viewport slices.

mod highlight;
mod input;
mod render;
mod styles;

use std::cell::RefCell;
use std::rc::Rc;

use rinch::prelude::*;
use rinch_code_editor_model::{EditorModel, Grid};

use styles::{
    EDITOR_STYLE, FALLBACK_CELL_WIDTH, LINE_HEIGHT, PROBE_LEN, PROBE_STYLE, TAB_SIZE, VIEW_STYLE,
    caret_style, current_line_style, gutter_style, layers_style, line_style, number_style,
    sel_rect_style,
};

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

    // `version` bumps on every model change; every reactive closure below reads it,
    // so one keystroke repaints exactly the rows and overlays that moved. `focused`
    // gates the keyboard interceptor so keys only reach us while the editor is active.
    let version = Signal::new(0u64);
    let focused = Signal::new(false);
    input::install(model.clone(), grid, version, focused);

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

    rsx! {
        div {
            style: EDITOR_STYLE,
            onmousedown: move || focused.set(true),

            // Gutter — its rows track the line count.
            div {
                style: gutter_style(),
                for row in gutter_rows(version, &model) {
                    div {
                        key: row.number,
                        style: number_style(row.top),
                        { row.number.to_string() }
                    }
                }
            }

            // Text view: a scroll container whose layer stack is sized to the full
            // document. Children are z-ordered: current-line band, selection, text,
            // caret.
            div {
                style: VIEW_STYLE,
                div {
                    style: { move || layers_style(content_h.get()) },

                    // Current-line band (z 0) — full width, no measurement needed.
                    div {
                        style: { move || current_line_style(current_line.get()) },
                    }

                    // Selection rectangles (z 1) — behind the glyphs.
                    for rect in selection_rects(version, &model, grid) {
                        div {
                            key: rect.key,
                            style: {
                                move || {
                                    sel_rect_style(rect.line, rect.start_vcol, rect.end_vcol, cell_w.get())
                                }
                            },
                        }
                    }

                    // Text rows (z 2).
                    for line in text_rows(version, &model, &default_color) {
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
            }
        }
    }
}

// ---- reactive reads --------------------------------------------------------
//
// Each of these reads `version` — establishing the reactive dependency — then
// returns freshly-computed render data. They exist as named functions rather
// than inline blocks because an rsx `for`'s iterator is parsed only up to the
// first `{`, so its expression cannot itself be a block; a call is unambiguous.

fn gutter_rows(version: Signal<u64>, model: &Rc<RefCell<EditorModel>>) -> Vec<render::GutterRow> {
    version.get();
    render::compute_gutter(&model.borrow())
}

fn selection_rects(
    version: Signal<u64>,
    model: &Rc<RefCell<EditorModel>>,
    grid: Grid,
) -> Vec<render::SelRect> {
    version.get();
    render::compute_sel_rects(&model.borrow(), &grid)
}

fn text_rows(
    version: Signal<u64>,
    model: &Rc<RefCell<EditorModel>>,
    default_color: &str,
) -> Vec<render::LineRender> {
    version.get();
    render::compute_lines(&mut model.borrow_mut(), default_color)
}
