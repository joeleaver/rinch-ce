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
        // Receiving a collaborator's edits while not recording your own is
        // never a coherent state: it means every local edit is silently failing
        // to reach the shared document, and the assertion above — which is the
        // guard against exactly that — would pass forever because the queue can
        // never fill. Catch the miswiring instead of diverging quietly.
        assert!(
            self.is_recording_local_changes(),
            "apply_remote on a model that is not recording local changes: call \
             record_local_changes(true) when attaching the collaborative document"
        );

        let lines_before = self.len_lines();
        let mut selections = self.selections().clone();
        // The region needing repaint, carried forward in the coordinates of
        // each successive splice.
        let mut extent: Option<(usize, usize)> = None;

        for splice in splices {
            let len_before = self.len_chars();
            // The single point where a splice off the network becomes an edit,
            // and so the single place it is clamped into the document. The
            // clamped range then steers the selection rebasing too, or the two
            // could disagree about what was replaced.
            let edit = splice.to_edit(len_before);
            let (pos, del) = (edit.range.start, edit.range.end - edit.range.start);

            // A splice that replaces the whole buffer is the collaborative
            // layer *resynchronizing*, not somebody's edit: the same document,
            // freshly stated.
            let resync = pos == 0 && del == len_before && del > 0;

            // …and a resync that restates the text we already hold is not news
            // at all. Applying it would be a no-op on the buffer while still
            // bumping the version, repainting everything, and — until C2b —
            // throwing away the undo history. The collaborative layer resyncs
            // on any patch it cannot project onto plain text, which a peer can
            // provoke at will (a block marker on every paragraph, say), so
            // this is the difference between "undo quietly stops working when a
            // certain peer is in the room" and nothing happening.
            if resync && edit.text == self.text() {
                continue;
            }

            let inserted = edit.text.chars().count();
            let change = self.apply_edit(edit);
            let len_after = self.len_chars();

            selections = if resync {
                // Every offset is "inside the replaced span", so the rule below
                // would collapse the whole selection set to 0 — a lie about
                // what happened. Carets hold their place instead, clamped.
                map_selections(&selections, len_after, |offset| offset)
            } else {
                map_selections(&selections, len_after, |offset| {
                    map_remote(offset, pos, del, inserted)
                })
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

        // Every splice was a restatement of what we already hold: nothing
        // happened, and saying otherwise would repaint the document and cost
        // the user their undo history for no reason.
        let (lo, hi) = extent?;

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

        Some(self.summarize_extent(lo, hi, EditKind::Remote, lines_before))
    }
}

/// Where an offset lands after a remote replacement of `del` chars at `pos` by
/// `inserted` chars. The arguments are the *clamped* ones the edit actually
/// used, so `pos + del` cannot overflow and no arm can underflow.
///
/// Biased left throughout — this is somebody else's edit and it must not drag
/// your caret across the screen:
///
/// - at or before the splice, an offset does not move at all, so text inserted
///   exactly at your caret appears *after* it;
/// - at or after the replaced span, it shifts by the splice's net length;
/// - strictly inside the replaced span, the text it addressed is gone, so it
///   collapses to the *start* of what replaced it.
fn map_remote(offset: usize, pos: usize, del: usize, inserted: usize) -> usize {
    let end = pos + del;
    if offset <= pos {
        offset
    } else if offset >= end {
        // offset >= pos + del, so subtracting del stays at or above pos.
        offset - del + inserted
    } else {
        pos
    }
}

/// Rebase every selection through `map`, clamped into a document of `len`
/// chars, re-establishing the sorted, non-overlapping invariant afterwards: two
/// carets a remote deletion collapsed onto the same offset are one caret now,
/// and must merge or the next edit applies twice.
///
/// The clamp is belt and braces — `map_remote` cannot leave the document for a
/// clamped splice — but a selection that escaped the buffer would survive
/// indefinitely and turn the *next local edit* into an out-of-range operation
/// on the shared document, which is far from where the mistake was made.
fn map_selections(selections: &Selections, len: usize, map: impl Fn(usize) -> usize) -> Selections {
    let items = selections
        .iter()
        .map(|sel| Selection {
            anchor: map(sel.anchor).min(len),
            head: map(sel.head).min(len),
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

    /// Rebase through a splice the way `apply_remote` does, in a document long
    /// enough that the clamp never fires.
    fn rebase(selections: &Selections, splice: &Splice) -> Selections {
        let inserted = splice.insert.chars().count();
        map_selections(selections, usize::MAX, |offset| {
            map_remote(offset, splice.pos, splice.del, inserted)
        })
    }

    #[test]
    fn an_insertion_at_the_caret_lands_after_it() {
        assert_eq!(map_remote(5, 5, 0, 2), 5, "the caret holds its place");
        assert_eq!(map_remote(4, 5, 0, 2), 4);
        assert_eq!(map_remote(6, 5, 0, 2), 8, "past it: shifted");
    }

    #[test]
    fn a_caret_inside_a_replaced_span_collapses_to_its_start() {
        // Replace 4 chars at offset 2 with 16.
        assert_eq!(map_remote(3, 2, 4, 16), 2);
        assert_eq!(map_remote(5, 2, 4, 16), 2);
        assert_eq!(map_remote(2, 2, 4, 16), 2, "at the leading edge: unmoved");
        assert_eq!(
            map_remote(6, 2, 4, 16),
            18,
            "at the trailing edge: after the new text"
        );
    }

    #[test]
    fn a_deletion_swallowing_a_selection_collapses_it_to_a_caret() {
        let mapped = rebase(&sel(3, 6), &Splice::new(1, 8, ""));
        assert_eq!(mapped.len(), 1);
        assert_eq!(*mapped.primary(), Selection::cursor(1));
    }

    #[test]
    fn selections_stay_sorted_and_merged_after_rebasing() {
        // Two carets either side of a deletion land on the same offset and
        // must become one, or the next edit is applied twice.
        let before = Selections::new(vec![Selection::cursor(2), Selection::cursor(7)], 1);
        let after = rebase(&before, &Splice::new(2, 5, ""));
        assert_eq!(after.len(), 1);
        assert_eq!(*after.primary(), Selection::cursor(2));
    }

    #[test]
    fn a_reversed_selection_keeps_its_direction() {
        let after = rebase(&sel(9, 4), &Splice::new(0, 0, "ab"));
        assert!(after.primary().is_reversed());
        assert_eq!(after.primary().anchor, 11);
        assert_eq!(after.primary().head, 6);
    }

    #[test]
    fn rebased_selections_are_clamped_into_the_document() {
        // A selection that escaped the buffer would survive indefinitely and
        // turn the next *local* edit into an out-of-range operation on the
        // shared document, a long way from where the mistake was made.
        let after = map_selections(&sel(3, 900), 10, |offset| offset);
        assert_eq!(after.primary().anchor, 3);
        assert_eq!(after.primary().head, 10);
    }
}
