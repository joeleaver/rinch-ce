//! A Monaco-parity code editor component for the rinch GUI framework.
//!
//! rinch is a *peer* dependency, not something this crate bundles — see the
//! crate README for why that is a diamond and not a cycle. Everything hard lives
//! in the three rinch-free crates below this one; here we only turn the
//! [`EditorModel`] into DOM.
//!
//! # Status: Increment 4a — static render
//!
//! This is the first rinch-dependent slice. It renders the document as a gutter
//! plus a stack of grid-positioned line rows, each row a run of coloured token
//! spans. It is intentionally read-only for now: the caret/selection overlays,
//! keyboard input, measured hit-testing, and viewport virtualization arrive in
//! the following sub-increments. The DOM structure it lays down — a gutter
//! column beside a `position: relative` view whose children are absolutely
//! grid-positioned — is the structure those layers slot into unchanged.

use rinch::prelude::*;
use rinch_code_editor_model::{
    EditorModel, Highlighter, PlainHighlighter, Rgba, Theme, TokenMetadata,
};

/// Line box height, in pixels. We own this value (it is set in CSS below), so
/// vertical positioning and, later, virtualization need no font measurement —
/// only horizontal hit-testing does, and that arrives with the overlays.
const LINE_HEIGHT: f32 = 20.0;
/// Gutter width in pixels. Wide enough for four digits at the current metrics.
const GUTTER_WIDTH: f32 = 52.0;
/// Left padding of the text area, matched by the caret/selection layers later.
const TEXT_PAD_LEFT: f32 = 8.0;

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

/// A gutter row: just the number and where it sits. Kept separate from
/// [`LineRender`] so the gutter loop does not clone the segment data.
#[derive(Clone, PartialEq)]
struct GutterRow {
    number: usize,
    top: f32,
}

/// The editor component. `text` is the initial document.
///
/// For this increment the component builds its own plain (unhighlighted) model;
/// a later increment lets the application supply a tree-sitter highlighter — or a
/// whole [`EditorModel`] — so the library never forces a grammar on its consumer.
#[component]
pub fn CodeEditor(text: String) -> NodeHandle {
    let theme = dark_theme();
    let default_color = theme
        .color(theme.default_metadata().foreground())
        .map(|c| c.to_css())
        .unwrap_or_else(|| "#d4d4d4".to_string());

    let highlighter: Box<dyn Highlighter> = Box::new(PlainHighlighter::new(theme));
    let mut model = EditorModel::from_text(&text, highlighter);

    let line_count = model.len_lines();
    let lines = compute_lines(&mut model, &default_color);
    let gutter: Vec<GutterRow> = (0..line_count)
        .map(|i| GutterRow {
            number: i + 1,
            top: i as f32 * LINE_HEIGHT,
        })
        .collect();
    let content_height = line_count as f32 * LINE_HEIGHT;

    rsx! {
        div {
            style: EDITOR_STYLE,

            // Gutter: a fixed-width column of right-aligned line numbers, each
            // grid-positioned to sit on its row.
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

            // Text view: a scroll container whose inner layer is sized to the
            // full document so the scrollbar reflects the whole thing.
            div {
                style: VIEW_STYLE,
                div {
                    style: text_layer_style(content_height),
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
                }
            }
        }
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

// ---- styles --------------------------------------------------------------
//
// Inline styles for now. When rinch stylesheet injection is wired in, these
// collapse to class names; the structure does not change.

const EDITOR_STYLE: &str = "position:relative; display:flex; width:100%; height:100%; \
    overflow:hidden; background:#1e1e1e; color:#d4d4d4; \
    font-family:'DejaVu Sans Mono','Cascadia Mono',monospace; font-size:14px; \
    line-height:20px; font-variant-ligatures:none;";

const VIEW_STYLE: &str = "position:relative; flex:1 1 auto; overflow:auto;";

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

fn text_layer_style(content_height: f32) -> String {
    format!("position:relative; height:{content_height}px;")
}

fn line_style(top: f32) -> String {
    format!(
        "position:absolute; left:{TEXT_PAD_LEFT}px; top:{top}px; \
         height:{LINE_HEIGHT}px; white-space:pre;"
    )
}
