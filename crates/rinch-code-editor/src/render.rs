//! Turning the model into render data.
//!
//! Every function here is a pure read of the [`EditorModel`] that returns a fresh
//! value — a list of rows, a caret position, a height. That shape is deliberate:
//! the component ([`crate::lib`]) calls each one inside a reactive closure that
//! also reads the document version signal, so a keystroke re-runs exactly these
//! reads and rinch's keyed reconciliation diffs the results against what is on
//! screen. Nothing here caches; the caching that matters (the highlighter's parse
//! tree) lives below, in the model.
//!
//! Everything is virtualized: the row-producing functions take the visible line
//! `Range` the component computes from the scroll position and only materialize
//! those lines. Row `top`s stay in *document* coordinates (`line * LINE_HEIGHT`),
//! so a row keyed by its line number sits at the same place whether or not it was
//! on screen last frame — native scroll (for the text) and a `translateY` (for the
//! gutter) do the rest. The caret and current-line band are single elements the
//! component renders unconditionally, so they are not sliced here.

use std::ops::Range;

use rinch_code_editor_model::{EditorModel, Grid, Theme, TokenMetadata};

use crate::styles::LINE_HEIGHT;

/// One coloured run within a rendered line.
#[derive(Clone, PartialEq)]
pub(crate) struct Seg {
    pub key: usize,
    pub text: String,
    pub color: String,
}

/// A line prepared for rendering: its 1-based number, its pixel top, and its
/// coloured segments.
#[derive(Clone, PartialEq)]
pub(crate) struct LineRender {
    pub number: usize,
    pub top: f32,
    pub segs: Vec<Seg>,
}

/// A gutter row: just the number and where it sits.
#[derive(Clone, PartialEq)]
pub(crate) struct GutterRow {
    pub number: usize,
    pub top: f32,
}

/// One selection rectangle, in grid coordinates (visual columns). The pixel width
/// is applied by [`crate::styles::sel_rect_style`] once the cell advance is measured.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct SelRect {
    pub key: usize,
    pub line: usize,
    pub start_vcol: usize,
    pub end_vcol: usize,
}

/// Turn the visible lines into per-line render data: each line's char text is
/// sliced by its token spans and each span coloured from the theme palette. Takes
/// `&mut` because asking the highlighter for a line's tokens may parse and cache.
/// Only `range` (already clamped to the document by the caller) is materialized.
pub(crate) fn compute_lines(
    model: &mut EditorModel,
    range: Range<usize>,
    default_color: &str,
) -> Vec<LineRender> {
    let end = range.end.min(model.len_lines());
    let mut out = Vec::with_capacity(end.saturating_sub(range.start));
    for i in range.start..end {
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

/// The gutter rows for the visible `range`. Tops are document coordinates; the
/// component's `translateY` slides them to match the text's native scroll.
pub(crate) fn compute_gutter(range: Range<usize>, line_count: usize) -> Vec<GutterRow> {
    let end = range.end.min(line_count);
    (range.start..end)
        .map(|i| GutterRow {
            number: i + 1,
            top: i as f32 * LINE_HEIGHT,
        })
        .collect()
}

/// The selection rectangles for every non-empty selection, clipped to the visible
/// `range`, in grid coordinates. Clipping matters: a select-all on a large file
/// would otherwise emit a rectangle per line. A line whose terminator is inside
/// the selection shows a one-cell sliver past its content, so a selected line
/// break reads as selected.
pub(crate) fn compute_sel_rects(
    model: &EditorModel,
    grid: &Grid,
    range: Range<usize>,
) -> Vec<SelRect> {
    let buffer = model.buffer();
    let mut rects = Vec::new();
    let mut key = 0;
    for sel in model.selections().iter() {
        if sel.is_empty() {
            continue;
        }
        let start = buffer.position_of(sel.start());
        let end = buffer.position_of(sel.end());
        let first = start.line.max(range.start);
        let last = end.line.min(range.end.saturating_sub(1));
        for line in first..=last {
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
            if line < end.line {
                end_vcol += 1;
            }
            rects.push(SelRect {
                key,
                line,
                start_vcol,
                end_vcol,
            });
            key += 1;
        }
    }
    rects
}

/// The primary caret's grid position: `(line, visual_column)`.
pub(crate) fn caret_grid(model: &EditorModel, grid: &Grid) -> (usize, usize) {
    let buffer = model.buffer();
    let head = buffer.position_of(model.selections().primary().head);
    let head_line_text = buffer.line_text(head.line);
    (head.line, grid.visual_column(&head_line_text, head.col))
}

/// The line the primary caret sits on — what the current-line band highlights.
pub(crate) fn caret_line(model: &EditorModel) -> usize {
    let head = model.selections().primary().head;
    model.buffer().position_of(head).line
}

/// Total pixel height of the document; the layer stack is sized to this.
pub(crate) fn content_height(model: &EditorModel) -> f32 {
    model.len_lines() as f32 * LINE_HEIGHT
}

/// A token's foreground as a CSS colour, falling back to the editor default when
/// the token carries no explicit colour (an unset palette slot).
fn color_of(theme: &Theme, metadata: TokenMetadata, default: &str) -> String {
    theme
        .color(metadata.foreground())
        .map(|c| c.to_css())
        .unwrap_or_else(|| default.to_string())
}
