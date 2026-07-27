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

use rinch_code_editor_model::Rgba;

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
/// Height of a collaborator's name chip, and its own line box — the editor's
/// 20px `line-height` would leave the label sitting off-centre in it.
pub(crate) const CHIP_HEIGHT: f32 = 16.0;
/// Diameter of the dot capping a remote caret. Centred on the 2px bar.
pub(crate) const DOT_SIZE: f32 = 6.0;
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

// ---- collaborators ---------------------------------------------------------
//
// Remote overlays share the local ones' z-order — tint behind the glyphs at 1,
// caret in front at 3 — and add a fourth layer for the caret's cap and name
// chip. Sharing the z-index means DOM order decides what covers what where two
// people select the same text; the component emits the remote loops after the
// local ones, so a collaborator's tint reads on top of your own.
//
// Every one of these takes its colours as [`Rgba`], not as a pre-rendered CSS
// string, and that is not a style preference. These are called from reactive
// `style:` blocks, which the rsx macro reconstructs on *every* effect run — so
// the block may only capture `Copy` values. An `Rgba` is `Copy`; the `String` a
// `to_css()` would have produced is not, and capturing one compiles the first
// run and fails to move out of the closure's environment on the second.

/// One collaborator's selection rectangle. Identical geometry to
/// [`sel_rect_style`] — deliberately, since both come from the same splitter —
/// differing only in taking the peer's translucent tint as its colour.
pub(crate) fn remote_sel_rect_style(
    line: usize,
    start_vcol: usize,
    end_vcol: usize,
    cw: f32,
    tint: Rgba,
) -> String {
    let tint = tint.to_css();
    format!(
        "position:absolute; z-index:1; top:{}px; left:{}px; width:{}px; \
         height:{LINE_HEIGHT}px; background:{tint}; pointer-events:none;",
        line as f32 * LINE_HEIGHT,
        TEXT_PAD_LEFT + start_vcol as f32 * cw,
        (end_vcol - start_vcol) as f32 * cw,
    )
}

/// Where a remote caret's left edge sits.
///
/// A caret at the *left* end of its own selection would otherwise be drawn on
/// top of that selection's tint — the same hue at full opacity over itself at
/// 35%, which is nearly invisible. Shifting it one bar-width out puts it
/// against the editor background instead. y-codemirror.next (`side: -1 | 1`) and
/// y-monaco (a `before` vs `after` decoration) both make the same adjustment.
fn remote_caret_x(vcol: usize, cw: f32, reversed: bool) -> f32 {
    let x = TEXT_PAD_LEFT + vcol as f32 * cw;
    if reversed { x - CARET_WIDTH } else { x }
}

/// A collaborator's caret bar. Never blinks: a blink says "this is where *your*
/// next keystroke lands", and none of the reference implementations animate a
/// remote one.
pub(crate) fn remote_caret_style(
    line: usize,
    vcol: usize,
    cw: f32,
    reversed: bool,
    color: Rgba,
) -> String {
    format!(
        "position:absolute; z-index:3; width:{CARET_WIDTH}px; height:{LINE_HEIGHT}px; \
         top:{}px; left:{}px; background:{}; pointer-events:none;",
        line as f32 * LINE_HEIGHT,
        remote_caret_x(vcol, cw, reversed),
        color.to_css(),
    )
}

/// The dot capping a remote caret — the persistent "somebody is here" marker,
/// rendered for exactly the peers whose name chip is *not* currently up.
///
/// Its top is clamped into the document rather than allowed its usual half-dot
/// overhang: the layer stack lives inside the scroll container, which clips
/// anything above the first line (a z-index does not escape a scrolling
/// ancestor).
pub(crate) fn remote_dot_style(
    line: usize,
    vcol: usize,
    cw: f32,
    reversed: bool,
    color: Rgba,
) -> String {
    let radius = DOT_SIZE / 2.0;
    format!(
        "position:absolute; z-index:4; width:{DOT_SIZE}px; height:{DOT_SIZE}px; \
         border-radius:{radius}px; top:{}px; left:{}px; background:{}; \
         pointer-events:none;",
        (line as f32 * LINE_HEIGHT - radius).max(0.0),
        remote_caret_x(vcol, cw, reversed) + CARET_WIDTH / 2.0 - radius,
        color.to_css(),
    )
}

/// A collaborator's name chip, sitting just above their caret.
///
/// On the first line it flips *below* instead, for the same clipping reason as
/// the dot: its natural position is outside the document. Live Share performs
/// the same flip.
pub(crate) fn remote_chip_style(
    line: usize,
    vcol: usize,
    cw: f32,
    reversed: bool,
    background: Rgba,
    color: Rgba,
) -> String {
    let top = line as f32 * LINE_HEIGHT;
    // Sit the chip's bottom edge exactly on the dot's top edge, so the label and
    // the marker it replaces occupy the same spot rather than overlapping.
    let gap = DOT_SIZE / 2.0;
    let top = if line == 0 {
        top + LINE_HEIGHT
    } else {
        top - CHIP_HEIGHT - gap
    };
    format!(
        "position:absolute; z-index:4; top:{top}px; left:{}px; \
         height:{CHIP_HEIGHT}px; line-height:{CHIP_HEIGHT}px; padding:0 4px; \
         border-radius:3px; font-size:11px; font-weight:600; white-space:nowrap; \
         pointer-events:none; background:{}; color:{};",
        remote_caret_x(vcol, cw, reversed) - 1.0,
        background.to_css(),
        color.to_css(),
    )
}
