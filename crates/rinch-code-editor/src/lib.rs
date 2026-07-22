//! A Monaco-parity code editor component for the rinch GUI framework.
//!
//! rinch is a *peer* dependency, not something this crate bundles — see the
//! crate README for why that is a diamond and not a cycle. Everything hard lives
//! in the three rinch-free crates below this one; here we only turn the
//! [`EditorModel`] into DOM.
//!
//! # Status: Increment 4b — overlays
//!
//! On top of the static render (4a), this adds the three grid-positioned overlay
//! layers a real editor needs: a current-line band behind everything, selection
//! rectangles behind the glyphs, and the caret in front. Their horizontal
//! placement needs the pixel advance of one character, which — unlike line
//! height, which we set ourselves — must be *measured*, since it differs between
//! the desktop (Parley) and web backends. We measure it once from a hidden probe
//! element via [`NodeHandle::bounds_signal`], and the overlay positions recompute
//! reactively the moment that measurement lands.
//!
//! Still read-only: a seed cursor+selection makes the layers visible. Keyboard
//! input (4c) will drive them from real edits and motion.

use rinch::prelude::*;
use rinch_code_editor_model::{
    EditorModel, Grid, Highlighter, PlainHighlighter, Rgba, Selection, Selections, Theme,
    TokenMetadata,
};

/// Line box height in pixels. We own this (it is the CSS `line-height` below), so
/// vertical positioning needs no measurement.
const LINE_HEIGHT: f32 = 20.0;
/// Gutter width in pixels. Wide enough for four digits at the current metrics.
const GUTTER_WIDTH: f32 = 52.0;
/// Left padding of the text area, shared by the text rows and the overlay layers
/// so glyphs and their caret/selection line up.
const TEXT_PAD_LEFT: f32 = 8.0;
/// Grid columns per tab stop.
const TAB_SIZE: usize = 4;
/// Caret bar width in pixels.
const CARET_WIDTH: f32 = 2.0;
/// Characters in the measurement probe; its width / this = one cell's advance.
const PROBE_LEN: usize = 10;
/// Cell width used before the probe has been measured (roughly DejaVu Sans Mono
/// at 14px); the real value snaps in on the first layout pass.
const FALLBACK_CELL_WIDTH: f32 = 8.4;

/// One coloured run within a rendered line.
#[derive(Clone, PartialEq)]
struct Seg {
    key: usize,
    text: String,
    color: String,
}

/// A line prepared for rendering: its 1-based number, its pixel top, and its
/// coloured segments.
#[derive(Clone, PartialEq)]
struct LineRender {
    number: usize,
    top: f32,
    segs: Vec<Seg>,
}

/// A gutter row: just the number and where it sits.
#[derive(Clone, PartialEq)]
struct GutterRow {
    number: usize,
    top: f32,
}

/// One selection rectangle, in grid coordinates (visual columns). The pixel width
/// is applied reactively once the cell advance is measured.
#[derive(Clone, Copy, PartialEq)]
struct SelRect {
    key: usize,
    line: usize,
    start_vcol: usize,
    end_vcol: usize,
}

/// Everything the overlay layers need, in grid coordinates.
struct Overlays {
    caret_line: usize,
    caret_vcol: usize,
    current_line: usize,
    sel_rects: Vec<SelRect>,
}

/// The editor component. `text` is the initial document.
#[component]
pub fn CodeEditor(text: String) -> NodeHandle {
    let theme = dark_theme();
    let default_color = theme
        .color(theme.default_metadata().foreground())
        .map(|c| c.to_css())
        .unwrap_or_else(|| "#d4d4d4".to_string());

    let highlighter: Box<dyn Highlighter> = Box::new(PlainHighlighter::new(theme));
    let mut model = EditorModel::from_text(&text, highlighter);

    // 4b: seed a caret + selection so the overlay layers are visible in the
    // read-only demo. Keyboard input (4c) replaces this with live editing.
    if let Some(sel) = demo_selection(&text) {
        model.set_selections(Selections::single(sel));
    }

    // A grid whose cell width is a placeholder (1.0): visual_column counts grid
    // cells and is independent of pixel width, which the overlays multiply in
    // reactively after measurement.
    let grid = Grid::new(1.0, LINE_HEIGHT, TAB_SIZE);

    let line_count = model.len_lines();
    let lines = compute_lines(&mut model, &default_color);
    let gutter: Vec<GutterRow> = (0..line_count)
        .map(|i| GutterRow {
            number: i + 1,
            top: i as f32 * LINE_HEIGHT,
        })
        .collect();
    let overlays = compute_overlays(&model, &grid);
    let content_height = line_count as f32 * LINE_HEIGHT;

    let caret_line = overlays.caret_line;
    let caret_vcol = overlays.caret_vcol;
    let current_line = overlays.current_line;
    let sel_rects = overlays.sel_rects;

    // A hidden probe of known length. Its measured width, divided by its length,
    // is the advance of one monospace cell. bounds_signal refreshes after each
    // layout pass, so reading it in the overlay closures makes them self-correct.
    let probe = __scope.create_element("span");
    probe.set_attribute("style", PROBE_STYLE);
    let probe_text = __scope.create_text(&"0".repeat(PROBE_LEN));
    probe.append_child(&probe_text);
    let probe_bounds = probe.bounds_signal();

    rsx! {
        div {
            style: EDITOR_STYLE,

            // Gutter.
            div {
                style: gutter_style(),
                for row in gutter.clone() {
                    div {
                        key: row.number,
                        style: number_style(row.top),
                        { row.number.to_string() }
                    }
                }
            }

            // Text view: a scroll container whose layer stack is sized to the full
            // document. Children are z-ordered: current-line band, selection,
            // text, caret.
            div {
                style: VIEW_STYLE,
                div {
                    style: layers_style(content_height),

                    // Current-line band (z 0) — full width, no measurement needed.
                    div { style: current_line_style(current_line) }

                    // Selection rectangles (z 1) — behind the glyphs.
                    for rect in sel_rects.clone() {
                        div {
                            key: rect.key,
                            style: {
                                let pb = probe_bounds;
                                move || sel_rect_style(rect, cell_width(pb))
                            },
                        }
                    }

                    // Text rows (z 2).
                    for line in lines.clone() {
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
                            let pb = probe_bounds;
                            move || caret_style(caret_line, caret_vcol, cell_width(pb))
                        },
                    }

                    // The measurement probe (hidden).
                    { probe }
                }
            }
        }
    }
}

/// Cell advance in pixels from the probe's measured bounds, with a fallback until
/// the first layout pass has produced a real measurement.
fn cell_width(probe_bounds: Signal<rinch::core::reactive::ElementBounds>) -> f32 {
    let width = probe_bounds.get().width;
    if width > 0.0 {
        width / PROBE_LEN as f32
    } else {
        FALLBACK_CELL_WIDTH
    }
}

/// Turn the model into per-line render data: each line's char text is sliced by
/// its token spans and each span coloured from the theme palette.
fn compute_lines(model: &mut EditorModel, default_color: &str) -> Vec<LineRender> {
    let line_count = model.len_lines();
    let mut out = Vec::with_capacity(line_count);
    for i in 0..line_count {
        let chars: Vec<char> = model.buffer().line_text(i).chars().collect();
        let tokens = model.line_tokens(i);
        let theme = model.theme();

        let mut segs = Vec::new();
        for (k, span) in tokens.iter().enumerate() {
            let start = span.start as usize;
            let end = (span.end as usize).min(chars.len());
            if start >= end {
                continue;
            }
            segs.push(Seg {
                key: k,
                text: chars[start..end].iter().collect(),
                color: color_of(theme, span.metadata, default_color),
            });
        }
        out.push(LineRender {
            number: i + 1,
            top: i as f32 * LINE_HEIGHT,
            segs,
        });
    }
    out
}

/// Compute the caret, current-line, and selection geometry from the model's
/// selections, all in grid coordinates (visual columns).
fn compute_overlays(model: &EditorModel, grid: &Grid) -> Overlays {
    let buffer = model.buffer();
    let selections = model.selections();

    let head = buffer.position_of(selections.primary().head);
    let head_line_text = buffer.line_text(head.line);
    let caret_vcol = grid.visual_column(&head_line_text, head.col);

    let mut sel_rects = Vec::new();
    let mut key = 0;
    for sel in selections.iter() {
        if sel.is_empty() {
            continue;
        }
        let start = buffer.position_of(sel.start());
        let end = buffer.position_of(sel.end());
        for line in start.line..=end.line {
            let line_text = buffer.line_text(line);
            let content_len = buffer.line_len(line);
            let s_col = if line == start.line { start.col } else { 0 };
            let e_col = if line == end.line {
                end.col
            } else {
                content_len
            };
            let start_vcol = grid.visual_column(&line_text, s_col);
            let mut end_vcol = grid.visual_column(&line_text, e_col);
            // A line whose terminator is inside the selection shows a sliver past
            // its content, so a selected line break reads as selected.
            if line < end.line {
                end_vcol += 1;
            }
            sel_rects.push(SelRect {
                key,
                line,
                start_vcol,
                end_vcol,
            });
            key += 1;
        }
    }

    Overlays {
        caret_line: head.line,
        caret_vcol,
        current_line: head.line,
        sel_rects,
    }
}

/// A token's foreground as a CSS colour, falling back to the editor default when
/// the token carries no explicit colour (an unset palette slot).
fn color_of(theme: &Theme, metadata: TokenMetadata, default: &str) -> String {
    theme
        .color(metadata.foreground())
        .map(|c| c.to_css())
        .unwrap_or_else(|| default.to_string())
}

/// The placeholder dark theme. Colours become configurable — and come from a
/// real syntax highlighter — in a later increment; the palette is data, not code.
fn dark_theme() -> Theme {
    Theme::builder()
        .default_foreground(Rgba::rgb(0xd4, 0xd4, 0xd4))
        .build()
}

/// Find a demonstration selection in the sample text (the first `greeting`), so
/// the read-only 4b build shows a caret and a selection.
fn demo_selection(text: &str) -> Option<Selection> {
    let byte = text.find("greeting")?;
    let start = text[..byte].chars().count();
    let end = start + "greeting".chars().count();
    Some(Selection::new(start, end))
}

// ---- styles --------------------------------------------------------------
//
// Inline styles for now. When rinch stylesheet injection is wired in, these
// collapse to class names; the structure does not change.

const EDITOR_STYLE: &str = "position:relative; display:flex; width:100%; height:100%; \
    overflow:hidden; background:#1e1e1e; color:#d4d4d4; \
    font-family:'DejaVu Sans Mono','Cascadia Mono',monospace; font-size:14px; \
    line-height:20px; font-variant-ligatures:none;";

const VIEW_STYLE: &str = "position:relative; flex:1 1 auto; overflow:auto;";

// Positioned far off to the left so it is laid out and measured (bounds_signal
// needs real layout) but never visible. `visibility:hidden` is not enough — it
// still occupied its spot on line 0 — and `display:none` would zero the bounds.
const PROBE_STYLE: &str = "position:absolute; top:0; left:-9999px; \
    white-space:pre; pointer-events:none;";

fn gutter_style() -> String {
    format!(
        "flex:0 0 {GUTTER_WIDTH}px; position:relative; overflow:hidden; \
         user-select:none; background:#1e1e1e;"
    )
}

fn number_style(top: f32) -> String {
    format!(
        "position:absolute; right:12px; top:{top}px; height:{LINE_HEIGHT}px; \
         color:#6e7681; text-align:right;"
    )
}

fn layers_style(content_height: f32) -> String {
    format!("position:relative; height:{content_height}px;")
}

fn line_style(top: f32) -> String {
    format!(
        "position:absolute; z-index:2; left:{TEXT_PAD_LEFT}px; top:{top}px; \
         height:{LINE_HEIGHT}px; white-space:pre;"
    )
}

fn current_line_style(line: usize) -> String {
    format!(
        "position:absolute; z-index:0; left:0; right:0; top:{}px; \
         height:{LINE_HEIGHT}px; background:#2a2d2e;",
        line as f32 * LINE_HEIGHT
    )
}

fn sel_rect_style(rect: SelRect, cw: f32) -> String {
    format!(
        "position:absolute; z-index:1; top:{}px; left:{}px; width:{}px; \
         height:{LINE_HEIGHT}px; background:#264f78;",
        rect.line as f32 * LINE_HEIGHT,
        TEXT_PAD_LEFT + rect.start_vcol as f32 * cw,
        (rect.end_vcol - rect.start_vcol) as f32 * cw,
    )
}

fn caret_style(line: usize, vcol: usize, cw: f32) -> String {
    format!(
        "position:absolute; z-index:3; width:{CARET_WIDTH}px; height:{LINE_HEIGHT}px; \
         top:{}px; left:{}px; background:#aeafad;",
        line as f32 * LINE_HEIGHT,
        TEXT_PAD_LEFT + vcol as f32 * cw,
    )
}
