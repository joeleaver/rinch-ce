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
/// Monaco's `stopRenderingLineAfter`: a line is drawn up to this many
/// characters and no further, however long it really is.
///
/// This is the whole of Monaco's answer to long lines — it does **no**
/// horizontal virtualization, no column windowing, nothing viewport-relative.
/// The cap is absolute, counted from column 1, and what follows it is replaced
/// by a "Show more" chip. Registered in VS Code's `editorOptions.ts` with no
/// schema description, so it is an API-level performance guard rather than a
/// user setting.
pub(crate) const STOP_RENDERING_LINE_AFTER: usize = 10_000;

/// Monaco's `scrollBeyondLastColumn`: extra cells of scrollable width past the
/// widest line, so the last character is not jammed against the right edge.
///
/// Four, not five — the registered default in `editorOptions.ts` is 4, and the
/// `IEditorOptions` JSDoc in the same file that says otherwise is stale.
pub(crate) const SCROLL_BEYOND_LAST_COLUMN: usize = 4;

/// Width of rinch's vertical scrollbar, which it paints as an **overlay** over
/// the right edge of a scroll container whose content overflows.
///
/// Two things need it, both because it overlays rather than insetting: a press
/// in that strip is a scrollbar grab and must not place a caret (rinch's own
/// `find_scrollbar_hit` uses the same width, and dispatches `data-onmousedown`
/// additively *before* its scrollbar logic, so stepping aside is on us — Monaco
/// likewise never moves the cursor on a scrollbar grab); and a horizontal
/// reveal must treat the strip as outside the viewport, or it parks the caret
/// underneath it. Monaco subtracts its own `verticalScrollbarWidth` in exactly
/// the same place.
pub(crate) const SCROLLBAR_WIDTH: f32 = 16.0;

/// Cells of scrollable width reserved for the "Show more" chip when a visible
/// line is truncated.
///
/// An allowance, not a measurement. The chip is an inline run whose width
/// depends on its own label ("Show more (4.9 KB)"), and the document width is
/// computed from grid arithmetic that never looks at rendered runs. Monaco does
/// not need this because it measures the laid-out line, chip included. Without
/// it the chip sits just past the maximum scroll offset — visible only in part,
/// and the one control that recovers the hidden text is the thing you cannot
/// reach. Over-reserving costs a little empty space at the far right, exactly
/// like [`SCROLL_BEYOND_LAST_COLUMN`].
pub(crate) const OVERFLOW_CHIP_CELLS: usize = 24;

/// Monaco's `ViewLines.HORIZONTAL_EXTRA_PX`: how far *left* of the caret a
/// non-minimal horizontal reveal pulls the viewport, so a caret scrolled into
/// view from the right is not pinned against the left edge.
pub(crate) const HORIZONTAL_EXTRA_PX: f32 = 30.0;

/// Monaco's `revealHorizontalRightPadding`: the matching room on the right.
///
/// Fifteen, not thirty — the registered default in `editorOptions.ts` is 15;
/// the `IEditorOptions` JSDoc beside it saying 30 is stale. The asymmetry is
/// Monaco's, and it is what makes typing towards the right edge scroll a little
/// ahead of the caret instead of jerking a whole half-viewport.
pub(crate) const REVEAL_HORIZONTAL_RIGHT_PADDING: f32 = 15.0;

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

/// The layer stack: the scroll spacer, sized to the whole document.
///
/// Both dimensions are explicit, and the width is not cosmetic. The rows are
/// absolutely positioned, and neither backend content-sizes a box to its
/// absolutely-positioned children — taffy excludes them from content-based
/// sizing, and rinch's desktop `compute_content_width` walks direct children
/// only. Without a width the stack measures exactly as wide as the viewport,
/// so the scroll container believes nothing overflows: no horizontal scrolling,
/// and a trackpad swipe over the editor falls through to scrolling the page.
///
/// It also gives [`current_line_style`]'s `left:0; right:0` something to resolve
/// against, so the current-line band spans the full scrollable width rather
/// than stopping at the viewport's right edge.
///
/// `min-width:100%` is Monaco's `scrollWidth = max(viewportWidth, contentWidth)`
/// expressed declaratively, and it is declarative on purpose: computing that
/// maximum needs the viewport's width, which measures 0 until the first layout
/// has run — so a computed floor is silently wrong on the first frame, and
/// nothing would recompute it until the first keystroke. Letting layout resolve
/// the percentage against the scroll container sidesteps the measurement
/// entirely.
pub(crate) fn layers_style(content_width: f32, content_height: f32) -> String {
    format!(
        "position:relative; width:{content_width}px; min-width:100%; \
         height:{content_height}px;"
    )
}

/// One coloured run of a line.
pub(crate) fn seg_style(color: &str) -> String {
    format!("color:{color}")
}

/// The chip that stands in for everything past
/// [`STOP_RENDERING_LINE_AFTER`] — Monaco's `.mtkoverflow`, which it styles as
/// a button because clicking it is what lifts the cap.
///
/// The colours are adapted rather than ported: Monaco's rule is written in
/// terms of `--vscode-button-background` and friends, and this editor has no
/// theme-token system to resolve those against yet. When one lands, this is a
/// token lookup like any other.
pub(crate) fn overflow_seg_style() -> String {
    format!(
        "background:#0e639c; color:#ffffff; border-radius:2px; padding:0 4px; \
         margin-left:{TEXT_PAD_LEFT}px; cursor:pointer;"
    )
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
