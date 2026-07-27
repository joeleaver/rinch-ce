//! Drawing the people on the other end of a shared document.
//!
//! The collaborative crate resolves who is present and where their carets are;
//! this module turns that answer into render data, in the same grid coordinates
//! the local selection and caret already use. It is the *view's* vocabulary for
//! presence, deliberately one layer below the view: the geometry is pure, so it
//! is unit-tested here rather than eyeballed in a screenshot.
//!
//! # Why this crate and not the collaborative one
//!
//! [`RemotePresence`] is not `rinch_code_editor_collab::Peer`, even though it
//! carries the same facts, and the editor crate does not depend on the
//! collaborative crate at all. That is the same split Monaco makes: the editor
//! renders decorations and knows nothing about who produced them, and the
//! collaboration binding — in our case the embedding application — maps its own
//! presence type into the editor's. An editor that had to know what a CRDT was
//! in order to draw a coloured caret would be the wrong shape.
//!
//! The hand-off costs nothing, because
//! [`Peer::selection`](rinch_code_editor_collab::Peer) is already a
//! [`Selection`] from the buffer crate — the very type this crate re-exports.
//!
//! # Why the presence source is not asked to resolve anything
//!
//! A remote caret's offsets are resolved against the document *as it stands now*
//! (the collaborative crate anchors them to operations, not to integers, and
//! re-resolves on read). So there is nothing here to rebase and nothing to keep
//! up to date across edits — which is precisely the job Monaco's decoration
//! engine exists to do, and precisely why we do not need one yet.

use std::ops::Range;

use rinch_code_editor_buffer::{Buffer, Selection};
use rinch_code_editor_syntax::Rgba;

use crate::geometry::Grid;
use crate::spans::{LineSpan, selection_spans};

/// How opaque a remote selection's tint is, as an alpha byte (0.35).
///
/// VS Code Live Share's value. It has to survive being layered over the editor
/// background *and* over the local selection, without the glyphs above it
/// losing contrast — 0.2 (y-codemirror.next's choice, tuned for a light theme)
/// disappears against our `#1e1e1e`.
pub const TINT_ALPHA: u8 = 89;

/// One collaborator, ready to draw.
///
/// `selection` is not optional: a peer who is in the document but has no caret
/// in it has nothing to render, so the mapping that produces these drops them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemotePresence {
    /// Stable for the life of that peer's document — the key to render by, and
    /// the key to diff one snapshot against the next.
    pub id: u64,
    pub name: String,
    pub color: Rgba,
    pub selection: Selection,
}

/// One rectangle of one collaborator's selection, in visual columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresenceRect {
    pub peer: u64,
    pub span: LineSpan,
    /// The peer's colour with [`TINT_ALPHA`] applied, ready for CSS.
    pub tint: Rgba,
}

/// One collaborator's caret: where it is, and what to label it with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresenceCaret {
    pub peer: u64,
    pub name: String,
    pub color: Rgba,
    /// A foreground that stays legible on `color` — see [`readable_on`].
    pub label_color: Rgba,
    pub line: usize,
    pub vcol: usize,
    /// True when the caret sits at the *left* edge of a selection (the head is
    /// before the anchor). The view shifts those out of the tint, so a caret is
    /// never drawn on top of its own selection colour — every reference
    /// implementation (y-codemirror.next's `side`, y-monaco's before/after)
    /// does the same.
    pub reversed: bool,
}

/// The selection rectangles for every collaborator, clipped to the `visible`
/// line window and ordered by peer id.
///
/// The ordering is not cosmetic: `peers()` is documented to return an
/// unspecified order, and a render whose DOM order changed frame to frame would
/// make overlapping tints flicker as they swapped depth.
pub fn presence_rects(
    peers: &[RemotePresence],
    buffer: &Buffer,
    grid: &Grid,
    visible: Range<usize>,
) -> Vec<PresenceRect> {
    let mut peers: Vec<&RemotePresence> = peers.iter().collect();
    peers.sort_by_key(|p| p.id);

    let mut out = Vec::new();
    for peer in peers {
        let tint = with_alpha(peer.color, TINT_ALPHA);
        out.extend(
            selection_spans(buffer, grid, peer.selection, visible.clone())
                .into_iter()
                .map(|span| PresenceRect {
                    peer: peer.id,
                    span,
                    tint,
                }),
        );
    }
    out
}

/// Every collaborator's caret whose line is on screen, ordered by peer id.
///
/// Carets outside the window are dropped rather than pinned to an edge. That is
/// parity, not laziness: no editor in the reference set renders an off-viewport
/// indicator for a remote caret, and an editor that did would put a permanent
/// row of chips along the top of any document with a peer reading the bottom
/// of it.
pub fn presence_carets(
    peers: &[RemotePresence],
    buffer: &Buffer,
    grid: &Grid,
    visible: Range<usize>,
) -> Vec<PresenceCaret> {
    if visible.is_empty() {
        return Vec::new();
    }
    let mut peers: Vec<&RemotePresence> = peers.iter().collect();
    peers.sort_by_key(|p| p.id);

    let len = buffer.len_chars();
    peers
        .into_iter()
        .filter_map(|peer| {
            // The offset can name text this buffer has not caught up with — the
            // collaborative document resolves against its own mirror. Clamp
            // rather than index past the end.
            let head = buffer.position_of(peer.selection.head.min(len));
            if !visible.contains(&head.line) {
                return None;
            }
            let line_text = buffer.line_text(head.line);
            Some(PresenceCaret {
                peer: peer.id,
                name: peer.name.clone(),
                color: peer.color,
                label_color: readable_on(peer.color),
                line: head.line,
                // The same formula the local caret uses
                // (`Grid::visual_column`), so the two can never disagree about
                // where a column is.
                vcol: grid.visual_column(&line_text, head.col),
                reversed: !peer.selection.is_empty() && peer.selection.is_reversed(),
            })
        })
        .collect()
}

/// `color` with its alpha replaced.
pub fn with_alpha(color: Rgba, alpha: u8) -> Rgba {
    Rgba::new(color.r, color.g, color.b, alpha)
}

/// Black or white, whichever stays legible on `bg`.
///
/// Live Share ships each participant colour as a foreground/background *pair*;
/// y-codemirror.next instead hard-codes white, which is unreadable on two of
/// the colours in its own default palette. Deriving the pair removes the
/// opportunity to get it wrong, and there is exactly one right answer per
/// background.
///
/// The threshold is WCAG's: compare the background's relative luminance against
/// 0.179, the crossover where black and white have equal contrast against it.
pub fn readable_on(bg: Rgba) -> Rgba {
    const BLACK: Rgba = Rgba::rgb(0x00, 0x00, 0x00);
    const WHITE: Rgba = Rgba::rgb(0xFF, 0xFF, 0xFF);
    if relative_luminance(bg) > 0.179 {
        BLACK
    } else {
        WHITE
    }
}

/// WCAG relative luminance, sRGB transfer function and all. Worth doing exactly:
/// the cheap `(r+g+b)/3` approximation puts saturated greens and blues on the
/// wrong side of the threshold, which is most of a collaboration palette.
fn relative_luminance(c: Rgba) -> f32 {
    fn linear(channel: u8) -> f32 {
        let v = channel as f32 / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * linear(c.r) + 0.7152 * linear(c.g) + 0.0722 * linear(c.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADA: u64 = 7;
    const GRACE: u64 = 3;

    fn grid() -> Grid {
        Grid::new(1.0, 20.0, 4)
    }

    fn peer(id: u64, name: &str, selection: Selection) -> RemotePresence {
        RemotePresence {
            id,
            name: name.to_string(),
            color: Rgba::rgb(0x30, 0xbc, 0xed),
            selection,
        }
    }

    #[test]
    fn a_peers_selection_becomes_tinted_rects() {
        let buffer = Buffer::from_text("hello world");
        let peers = [peer(ADA, "Ada", Selection::new(2, 7))];
        let rects = presence_rects(&peers, &buffer, &grid(), 0..1);

        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].peer, ADA);
        assert_eq!(rects[0].span.start_vcol, 2);
        assert_eq!(rects[0].span.end_vcol, 7);
        assert_eq!(rects[0].tint.a, TINT_ALPHA);
        assert_eq!(
            (rects[0].tint.r, rects[0].tint.g, rects[0].tint.b),
            (0x30, 0xbc, 0xed),
            "the tint keeps the peer's hue, only the alpha moves"
        );
    }

    #[test]
    fn a_peer_with_a_bare_caret_has_no_rects_but_still_has_a_caret() {
        let buffer = Buffer::from_text("hello world");
        let peers = [peer(ADA, "Ada", Selection::cursor(4))];

        assert!(presence_rects(&peers, &buffer, &grid(), 0..1).is_empty());

        let carets = presence_carets(&peers, &buffer, &grid(), 0..1);
        assert_eq!(carets.len(), 1);
        assert_eq!(carets[0].vcol, 4);
        assert!(!carets[0].reversed);
    }

    #[test]
    fn the_caret_is_drawn_at_the_head_whichever_end_that_is() {
        let buffer = Buffer::from_text("hello world");
        let grid = grid();

        let forward = presence_carets(
            &[peer(ADA, "Ada", Selection::new(2, 7))],
            &buffer,
            &grid,
            0..1,
        );
        assert_eq!(forward[0].vcol, 7);
        assert!(
            !forward[0].reversed,
            "a forward selection's caret is already outside the tint"
        );

        let backward = presence_carets(
            &[peer(ADA, "Ada", Selection::new(7, 2))],
            &buffer,
            &grid,
            0..1,
        );
        assert_eq!(backward[0].vcol, 2);
        assert!(
            backward[0].reversed,
            "a backward selection's caret must be shifted clear of its own tint"
        );
    }

    #[test]
    fn a_caret_outside_the_window_is_not_rendered() {
        let buffer = Buffer::from_text("a\nb\nc\nd\ne\n");
        let peers = [peer(ADA, "Ada", Selection::cursor(0))];
        assert_eq!(presence_carets(&peers, &buffer, &grid(), 0..2).len(), 1);
        assert!(presence_carets(&peers, &buffer, &grid(), 2..4).is_empty());
    }

    #[test]
    fn an_empty_window_renders_nothing_at_all() {
        let buffer = Buffer::from_text("hello");
        let peers = [peer(ADA, "Ada", Selection::new(0, 5))];
        assert!(presence_carets(&peers, &buffer, &grid(), 0..0).is_empty());
        assert!(presence_rects(&peers, &buffer, &grid(), 0..0).is_empty());
    }

    /// `peers()` promises no order. Two frames that disagreed about it would
    /// reshuffle the DOM and flicker overlapping tints.
    #[test]
    fn output_is_ordered_by_peer_id_whatever_order_the_input_arrives_in() {
        let buffer = Buffer::from_text("hello world");
        let ada = peer(ADA, "Ada", Selection::new(0, 3));
        let grace = peer(GRACE, "Grace", Selection::new(4, 6));

        for input in [
            vec![ada.clone(), grace.clone()],
            vec![grace.clone(), ada.clone()],
        ] {
            let rects = presence_rects(&input, &buffer, &grid(), 0..1);
            assert_eq!(
                rects.iter().map(|r| r.peer).collect::<Vec<_>>(),
                [GRACE, ADA]
            );
            let carets = presence_carets(&input, &buffer, &grid(), 0..1);
            assert_eq!(
                carets.iter().map(|c| c.peer).collect::<Vec<_>>(),
                [GRACE, ADA]
            );
        }
    }

    /// A collaborator's anchor can outlive the text it named, and the
    /// collaborative document resolves against its own mirror — which may be
    /// momentarily ahead of this buffer. The renderer must not be the thing
    /// that notices.
    #[test]
    fn offsets_past_the_end_of_this_buffer_do_not_panic() {
        let buffer = Buffer::from_text("abc");
        let peers = [peer(ADA, "Ada", Selection::new(900, 901))];
        let carets = presence_carets(&peers, &buffer, &grid(), 0..1);
        assert_eq!(carets.len(), 1);
        assert_eq!((carets[0].line, carets[0].vcol), (0, 3));
    }

    #[test]
    fn a_multi_line_remote_selection_splits_per_line() {
        let buffer = Buffer::from_text("ab\ncd\nef");
        let peers = [peer(ADA, "Ada", Selection::new(1, 7))];
        let rects = presence_rects(&peers, &buffer, &grid(), 0..3);
        assert_eq!(
            rects.iter().map(|r| r.span.line).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!(rects.iter().all(|r| r.peer == ADA));
    }

    #[test]
    fn label_colour_is_the_legible_one() {
        // A bright collaboration colour needs black text; a dark one needs white.
        assert_eq!(readable_on(Rgba::rgb(0xec, 0xd4, 0x44)).r, 0x00);
        assert_eq!(readable_on(Rgba::rgb(0x1b, 0xe7, 0xff)).r, 0x00);
        assert_eq!(readable_on(Rgba::rgb(0x30, 0xbc, 0xed)).r, 0x00);
        assert_eq!(readable_on(Rgba::rgb(0x6f, 0x00, 0xff)).r, 0xFF);
        assert_eq!(readable_on(Rgba::rgb(0x00, 0x00, 0x00)).r, 0xFF);
        assert_eq!(readable_on(Rgba::rgb(0xff, 0xff, 0xff)).r, 0x00);
    }

    /// The cheap average-of-channels approximation gets this one wrong: pure
    /// green averages to 0x55 (dark, "use white") but is in fact the brightest
    /// primary and needs black text.
    #[test]
    fn luminance_is_perceptual_not_an_average_of_channels() {
        assert_eq!(readable_on(Rgba::rgb(0x00, 0xff, 0x00)).r, 0x00);
        assert_eq!(readable_on(Rgba::rgb(0x00, 0x00, 0xff)).r, 0xFF);
    }

    #[test]
    fn a_caret_carries_what_the_chip_needs_to_draw_itself() {
        let buffer = Buffer::from_text("hello");
        let carets = presence_carets(
            &[peer(ADA, "Ada", Selection::cursor(1))],
            &buffer,
            &grid(),
            0..1,
        );
        assert_eq!(carets[0].name, "Ada");
        assert_eq!(carets[0].color, Rgba::rgb(0x30, 0xbc, 0xed));
        assert_eq!(carets[0].label_color, readable_on(carets[0].color));
    }
}
