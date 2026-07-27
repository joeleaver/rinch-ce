//! Replaying a collaborator's edits into the model.
//!
//! The counterpart to [`EditorModel::commit`](crate::EditorModel): edits that
//! arrive from a shared document instead of from this keyboard. They differ in
//! four ways, and each one is a deliberate decision rather than an omission —
//! no undo entry, no publication back to the document, no caret reveal, and a
//! *left* bias when rebasing live selections.
//!
//! The collaborative layer itself lives one crate over and knows nothing about
//! this one; all that crosses the boundary is [`Splice`], which the buffer
//! crate owns.

use rinch_code_editor_buffer::{Bias, Selection, Selections, Splice};

use crate::model::{EditKind, EditSummary, EditorModel};

impl EditorModel {
    /// Apply edits a collaborator made, as delivered by the collaborative
    /// layer. Returns `None` — changing nothing, not even the version — for an
    /// empty batch.
    ///
    /// The splices are **sequential** (see [`Splice`]): each addresses the text
    /// the ones before it produced, so they land one at a time, each as its own
    /// atomic change. The undo history is untouched, because you cannot undo
    /// somebody else's typing, and nothing here is queued for publication,
    /// because these edits came *from* the shared document.
    ///
    /// Live selections ride along, biased left throughout: text inserted
    /// exactly at your caret lands *after* it, and a caret inside a span a
    /// collaborator replaced collapses to the start of the replacement rather
    /// than riding to its end. That is precisely the difference between the
    /// caret that did the typing and a caret somebody typed over — see
    /// [`Bias`].
    ///
    /// The returned summary is [`EditKind::Remote`], which the view must not
    /// treat as a reason to scroll.
    ///
    /// # Ordering
    ///
    /// Every local change must reach the collaborative document *before* this
    /// is called (see
    /// [`take_local_changes`](EditorModel::take_local_changes)). These splices
    /// were computed against a document that has already absorbed your local
    /// edits; replaying them onto a buffer holding edits the document has never
    /// seen puts every offset in this batch in the wrong place. That is an
    /// integration bug rather than a recoverable condition, so it asserts —
    /// in release builds too, because silent divergence is the one outcome a
    /// collaborative editor must never have.
    pub fn apply_remote(&mut self, splices: &[Splice]) -> Option<EditSummary> {
        if splices.is_empty() {
            return None;
        }
        assert!(
            !self.has_local_changes(),
            "apply_remote with unpublished local changes: hand them to the \
             collaborative document first, or every offset in this batch is stale"
        );

        let lines_before = self.len_lines();
        let mut selections = self.selections().clone();
        // The region needing repaint, carried forward in the coordinates of
        // each successive splice.
        let mut extent: Option<(usize, usize)> = None;

        for splice in splices {
            // A splice that replaces the whole buffer is the collaborative
            // layer *resynchronizing*, not somebody's edit: same document,
            // freshly stated. Collapsing every selection to offset 0 — what
            // the rule below would do, since every offset is "inside the
            // replaced span" — would be a lie about what happened, so carets
            // hold their place, clamped into the new length.
            let resync = splice.pos == 0 && splice.del == self.len_chars() && splice.del > 0;

            let change = self.apply_splice(splice);

            selections = if resync {
                let len = self.len_chars();
                map_selections(&selections, |offset| offset.min(len))
            } else {
                map_selections(&selections, |offset| map_remote(offset, splice))
            };

            let (mut lo, mut hi) = match extent {
                Some((lo, hi)) => (
                    change.map_offset(lo, Bias::Before),
                    change.map_offset(hi, Bias::After),
                ),
                None => (usize::MAX, 0),
            };
            for edit in change.inverse() {
                lo = lo.min(edit.range.start);
                hi = hi.max(edit.range.end);
            }
            extent = Some((lo, hi));
        }

        // A remote arrival ends the open typing run: the next character you
        // type is its own undo step, not a continuation of one recorded
        // against text that has since moved.
        self.set_selections(selections);

        // C2b replaces this with rebasing the stacks through every remote
        // change (splitting stored ranges around remote insertions, so undo
        // never removes a collaborator's text). Until then, forgetting is the
        // only honest option: the recorded edits no longer describe this
        // buffer, and replaying them would corrupt it.
        self.clear_history();

        let (lo, hi) = extent.expect("a non-empty batch produced no extent");
        Some(self.summarize_extent(lo, hi, EditKind::Remote, lines_before))
    }
}

/// Where an offset lands after a remote splice.
///
/// Biased left throughout — this is somebody else's edit and it must not drag
/// your caret across the screen:
///
/// - at or before the splice, an offset does not move at all, so text inserted
///   exactly at your caret appears *after* it;
/// - at or after the replaced span, it shifts by the splice's net length;
/// - strictly inside the replaced span, the text it addressed is gone, so it
///   collapses to the *start* of what replaced it.
fn map_remote(offset: usize, splice: &Splice) -> usize {
    let end = splice.pos + splice.del;
    if offset <= splice.pos {
        offset
    } else if offset >= end {
        // offset >= pos + del, so adding (insert - del) cannot go negative.
        (offset as isize + splice.len_delta()) as usize
    } else {
        splice.pos
    }
}

/// Rebase every selection through `map`, re-establishing the sorted,
/// non-overlapping invariant afterwards: two carets a remote deletion collapsed
/// onto the same offset are one caret now, and must merge or the next edit
/// applies twice.
fn map_selections(selections: &Selections, map: impl Fn(usize) -> usize) -> Selections {
    let items = selections
        .iter()
        .map(|sel| Selection {
            anchor: map(sel.anchor),
            head: map(sel.head),
            // The remembered column survives: a remote edit elsewhere in the
            // document is no reason to forget where a vertical motion run was
            // headed.
            goal_col: sel.goal_col,
        })
        .collect();
    Selections::new(items, selections.primary_index())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(anchor: usize, head: usize) -> Selections {
        Selections::single(Selection::new(anchor, head))
    }

    #[test]
    fn an_insertion_at_the_caret_lands_after_it() {
        let splice = Splice::new(5, 0, "XY");
        assert_eq!(map_remote(5, &splice), 5, "the caret holds its place");
        assert_eq!(map_remote(4, &splice), 4);
        assert_eq!(map_remote(6, &splice), 8, "past it: shifted");
    }

    #[test]
    fn a_caret_inside_a_replaced_span_collapses_to_its_start() {
        let splice = Splice::new(2, 4, "long replacement");
        assert_eq!(map_remote(3, &splice), 2);
        assert_eq!(map_remote(5, &splice), 2);
        assert_eq!(map_remote(2, &splice), 2, "at the leading edge: unmoved");
        assert_eq!(
            map_remote(6, &splice),
            18,
            "at the trailing edge: after the new text"
        );
    }

    #[test]
    fn a_deletion_swallowing_a_selection_collapses_it_to_a_caret() {
        let splice = Splice::new(1, 8, "");
        let mapped = map_selections(&sel(3, 6), |o| map_remote(o, &splice));
        assert_eq!(mapped.len(), 1);
        assert_eq!(*mapped.primary(), Selection::cursor(1));
    }

    #[test]
    fn selections_stay_sorted_and_merged_after_rebasing() {
        // Two carets either side of a deletion land on the same offset and
        // must become one, or the next edit is applied twice.
        let splice = Splice::new(2, 5, "");
        let before = Selections::new(vec![Selection::cursor(2), Selection::cursor(7)], 1);
        let after = map_selections(&before, |o| map_remote(o, &splice));
        assert_eq!(after.len(), 1);
        assert_eq!(*after.primary(), Selection::cursor(2));
    }

    #[test]
    fn a_reversed_selection_keeps_its_direction() {
        let splice = Splice::new(0, 0, "ab");
        let after = map_selections(&sel(9, 4), |o| map_remote(o, &splice));
        assert!(after.primary().is_reversed());
        assert_eq!(after.primary().anchor, 11);
        assert_eq!(after.primary().head, 6);
    }
}
