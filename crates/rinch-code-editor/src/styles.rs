//! The editor's metrics and inline styles, in one place.
//!
//! Two kinds of thing live here: the pixel constants that define the grid the
//! whole view is laid out on (line height, gutter width, padding), and the small
//! functions that turn a row or overlay's grid position into a CSS string. Both
//! the reactive render ([`crate::render`]) and the component wiring
//! ([`crate::lib`]) read the constants, so keeping them here means one edit
//! changes the geometry everywhere it is used.
//!
//! The styles are inline for now. When rinch stylesheet injection is wired in,
//! the per-node functions collapse to class names and the constants become CSS
//! custom properties; the structure does not change.

/// Line box height in pixels. We own this (it is the CSS `line-height` below), so
/// vertical positioning needs no measurement.
pub(crate) const LINE_HEIGHT: f32 = 20.0;
/// Gutter width in pixels. Wide enough for four digits at the current metrics.
pub(crate) const GUTTER_WIDTH: f32 = 52.0;
/// Left padding of the text area, shared by the text rows and the overlay layers
/// so glyphs and their caret/selection line up.
pub(crate) const TEXT_PAD_LEFT: f32 = 8.0;
/// Grid columns per tab stop.
pub(crate) const TAB_SIZE: usize = 4;
/// Caret bar width in pixels.
pub(crate) const CARET_WIDTH: f32 = 2.0;
/// Characters in the measurement probe; its width / this = one cell's advance.
pub(crate) const PROBE_LEN: usize = 10;
/// Cell width used before the probe has been measured (roughly DejaVu Sans Mono
/// at 14px); the real value snaps in on the first layout pass.
pub(crate) const FALLBACK_CELL_WIDTH: f32 = 8.4;

pub(crate) const EDITOR_STYLE: &str = "position:relative; display:flex; width:100%; height:100%; \
    overflow:hidden; background:#1e1e1e; color:#d4d4d4; \
    font-family:'DejaVu Sans Mono','Cascadia Mono',monospace; font-size:14px; \
    line-height:20px; font-variant-ligatures:none;";

pub(crate) const VIEW_STYLE: &str = "position:relative; flex:1 1 auto; overflow:auto;";

// Positioned far off to the left so it is laid out and measured (bounds_signal
// needs real layout) but never visible. `visibility:hidden` is not enough — it
// still occupied its spot on line 0 — and `display:none` would zero the bounds.
pub(crate) const PROBE_STYLE: &str = "position:absolute; top:0; left:-9999px; \
    white-space:pre; pointer-events:none;";

pub(crate) fn gutter_style() -> String {
    format!(
        "flex:0 0 {GUTTER_WIDTH}px; position:relative; overflow:hidden; \
         user-select:none; background:#1e1e1e;"
    )
}

/// The gutter's inner band, shifted up by the scroll offset so its numbers —
/// laid out in document coordinates — track the text's native scroll. The text
/// scrolls itself (its container has `overflow:auto`); the gutter, which sits
/// outside that container, is moved here instead.
///
/// The shift is a layout `top`, deliberately NOT a `transform`: rinch's
/// dirty-region tracking uses untransformed layout rects, so a translated
/// band's numbers get culled against their pre-transform positions and vanish
/// on any partial repaint (they only survived before because caret moves
/// forced full repaints). A layout offset moves the real rects, so every
/// paint path sees the numbers where they are.
///
/// The explicit `height` (the full document height, like the layer stack's)
/// guards rinch's zero-dimension paint fast path, which mishandles elements
/// whose auto height collapses to 0 over absolutely-positioned children.
pub(crate) fn gutter_scroll_style(scroll_top: f64, content_h: f32) -> String {
    format!(
        "position:absolute; top:{}px; left:0; right:0; height:{content_h}px;",
        -scroll_top
    )
}

pub(crate) fn number_style(top: f32) -> String {
    format!(
        "position:absolute; right:12px; top:{top}px; height:{LINE_HEIGHT}px; \
         color:#6e7681; text-align:right;"
    )
}

pub(crate) fn layers_style(content_height: f32) -> String {
    format!("position:relative; height:{content_height}px;")
}

pub(crate) fn line_style(top: f32) -> String {
    format!(
        "position:absolute; z-index:2; left:{TEXT_PAD_LEFT}px; top:{top}px; \
         height:{LINE_HEIGHT}px; white-space:pre;"
    )
}

pub(crate) fn current_line_style(line: usize) -> String {
    format!(
        "position:absolute; z-index:0; left:0; right:0; top:{}px; \
         height:{LINE_HEIGHT}px; background:#2a2d2e;",
        line as f32 * LINE_HEIGHT
    )
}

/// One selection rectangle, from its line and its start/end visual columns. The
/// pixel horizontal placement multiplies in `cw`, the measured cell advance.
pub(crate) fn sel_rect_style(line: usize, start_vcol: usize, end_vcol: usize, cw: f32) -> String {
    format!(
        "position:absolute; z-index:1; top:{}px; left:{}px; width:{}px; \
         height:{LINE_HEIGHT}px; background:#264f78;",
        line as f32 * LINE_HEIGHT,
        TEXT_PAD_LEFT + start_vcol as f32 * cw,
        (end_vcol - start_vcol) as f32 * cw,
    )
}

/// The caret bar, from its line and visual column. `cw` is the measured cell
/// advance the horizontal position multiplies in; `shown` is the blink/focus
/// gate — hiding via `display` (not removal) keeps the node cheap to toggle.
pub(crate) fn caret_style(line: usize, vcol: usize, cw: f32, shown: bool) -> String {
    format!(
        "position:absolute; z-index:3; width:{CARET_WIDTH}px; height:{LINE_HEIGHT}px; \
         top:{}px; left:{}px; background:#aeafad;{}",
        line as f32 * LINE_HEIGHT,
        TEXT_PAD_LEFT + vcol as f32 * cw,
        if shown { "" } else { " display:none;" },
    )
}
