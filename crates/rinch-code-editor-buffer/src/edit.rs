use crate::buffer::Buffer;
use std::ops::Range;

/// Which side of a same-position insertion an offset ends up on.
///
/// When text is inserted at exactly the offset being rebased, there is no
/// single right answer, only a choice about intent:
///
/// - [`Bias::After`] — the caret that *did* the typing. It must ride along to
///   the end of what it just inserted, or typing would run backwards.
/// - [`Bias::Before`] — a caret that happens to sit where someone *else* typed.
///   It stays put, so a collaborator typing at your position does not drag your
///   cursor across the screen.
///
/// The same distinction governs whether a marker (a diagnostic, a fold, a remote
/// caret) grows to include text inserted at its edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bias {
    Before,
    After,
}

/// Replace `range` with `text`. Offsets are chars, in the coordinates of the
/// document *before* the enclosing [`Change`] is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
}

impl Edit {
    pub fn insert(at: usize, text: impl Into<String>) -> Self {
        Self {
            range: at..at,
            text: text.into(),
        }
    }

    pub fn delete(range: Range<usize>) -> Self {
        Self {
            range,
            text: String::new(),
        }
    }

    pub fn replace(range: Range<usize>, text: impl Into<String>) -> Self {
        Self {
            range,
            text: text.into(),
        }
    }

    fn inserted_len(&self) -> usize {
        self.text.chars().count()
    }
}

/// Delete `del` chars at `pos`, then insert `insert` there — one contiguous
/// replacement, in char offsets.
///
/// The difference from [`Edit`] is *coordinates*, and it is the whole reason
/// this type exists. The edits inside one [`Change`] all address the document
/// as it was **before** the change, and land together. A batch of splices is
/// **sequential**: each addresses the document the splices before it produced,
/// so they must be applied one at a time, in order. That is how a CRDT reports
/// what a collaborator did, and how the collaborative layer hands remote edits
/// to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Splice {
    pub pos: usize,
    pub del: usize,
    pub insert: String,
}

impl Splice {
    pub fn new(pos: usize, del: usize, insert: impl Into<String>) -> Self {
        Self {
            pos,
            del,
            insert: insert.into(),
        }
    }

    /// The equivalent [`Edit`], clamped into a document of `len` chars.
    ///
    /// Only ever for a change of *its own*: putting two splices' edits in one
    /// [`Change`] would apply them in shared pre-change coordinates and
    /// misplace the second one.
    ///
    /// The length is not optional, because a splice generally arrives from the
    /// network and nothing upstream of here has to be honest. `pos + del` on
    /// unvalidated input can wrap, and an edit built from the wrapped value
    /// corrupts the change it lands in (see [`Change::apply`]'s totality note).
    pub fn to_edit(&self, len: usize) -> Edit {
        let pos = self.pos.min(len);
        let del = self.del.min(len - pos);
        Edit::replace(pos..pos + del, self.insert.clone())
    }

    /// Net change in char count.
    pub fn len_delta(&self) -> isize {
        self.insert.chars().count() as isize - self.del as isize
    }
}

/// A set of edits applied as one atomic unit, together with the edits that undo it.
///
/// Multi-cursor typing produces one edit per cursor, and they must land together:
/// applying them one at a time invalidates every offset after the first. The
/// running-delta application below is what keeps the remaining offsets honest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    edits: Vec<Edit>,
    inverse: Vec<Edit>,
}

impl Change {
    pub fn edits(&self) -> &[Edit] {
        &self.edits
    }

    pub fn inverse(&self) -> &[Edit] {
        &self.inverse
    }

    /// Apply `edits` to `buffer` atomically and return the resulting [`Change`].
    ///
    /// `edits` may arrive in any order; they are sorted by start offset — with a
    /// *stable* sort, so two zero-width edits at the same offset keep the order
    /// the caller gave them and their texts land in that order. They must not
    /// overlap — overlapping edits from two cursors are a caller bug, and
    /// [`crate::Selections`] already guarantees non-overlap by construction.
    ///
    /// # Totality
    ///
    /// A malformed edit — out of range, or with `end` before `start` — is
    /// clamped into the document rather than trusted. That is not politeness:
    /// [`Buffer`] already clamps its own operations, so such an edit could never
    /// panic, but the running delta below computed from the *requested* range
    /// would then desynchronize every later edit in this change, and the
    /// recorded `inverse` would describe a replacement that never happened.
    /// Downstream of that, a collaborative session turns the bad arithmetic into
    /// a negative deletion length and deletes *backwards* in the shared
    /// document, on every peer. In release builds — which this project uses, and
    /// where the assertion below does not exist and `usize` subtraction wraps
    /// silently — the delta is the only thing standing between one rejected edit
    /// and a corrupted document. So the walk takes its delta from what was
    /// actually replaced, and for well-formed edits that is the same number.
    pub fn apply(buffer: &mut Buffer, mut edits: Vec<Edit>) -> Change {
        // STABLE, load-bearing: two zero-width edits at the same offset sort
        // equal, and their texts must land in the order the caller emitted
        // them. `sort_unstable_by_key` here would silently reorder them.
        edits.sort_by_key(|e| (e.range.start, e.range.end));
        debug_assert!(
            edits.iter().all(|e| e.range.start <= e.range.end),
            "inverted edit range: {edits:?}"
        );
        debug_assert!(
            edits.windows(2).all(|w| w[0].range.end <= w[1].range.start),
            "overlapping edits in one change: {edits:?}"
        );

        let mut inverse = Vec::with_capacity(edits.len());
        // Walk forward, carrying the net offset shift from every edit already
        // applied. Each edit's own range is in pre-change coordinates, so it needs
        // the accumulated delta added; everything before it is already settled.
        let mut delta: isize = 0;

        for edit in &edits {
            let len = buffer.len_chars() as isize;
            let start = (edit.range.start as isize + delta).clamp(0, len);
            let end = (edit.range.end as isize + delta).clamp(start, len);
            let (start, end) = (start as usize, end as usize);

            let removed = buffer.slice(start..end);
            if start != end {
                buffer.remove(start..end);
            }
            if !edit.text.is_empty() {
                buffer.insert(start, &edit.text);
            }

            let inserted = edit.inserted_len();
            // Recorded in post-change coordinates. Every later edit shifts only
            // content after this point, so this range stays valid to the end.
            inverse.push(Edit {
                range: start..start + inserted,
                text: removed,
            });

            delta += inserted as isize - (end - start) as isize;
        }

        Change { edits, inverse }
    }

    /// Re-apply this change to a buffer in its pre-change state (redo).
    ///
    /// Returns the change that was *applied*, not this one: reverting is itself
    /// an edit, and a collaborative session has to publish it like any other.
    pub fn redo(&self, buffer: &mut Buffer) -> Change {
        Self::apply(buffer, self.edits.clone())
    }

    /// Revert this change on a buffer in its post-change state (undo).
    /// Returns the change that was applied — see [`Change::redo`].
    pub fn undo(&self, buffer: &mut Buffer) -> Change {
        Self::apply(buffer, self.inverse.clone())
    }

    /// Rebase a pre-change offset into post-change coordinates.
    ///
    /// An offset inside a deleted region collapses to the start of whatever
    /// replaced it — the same thing every editor does when your cursor sits in
    /// text a collaborator just removed.
    pub fn map_offset(&self, offset: usize, bias: Bias) -> usize {
        let mut result = offset as isize;
        for edit in &self.edits {
            let (start, end) = (edit.range.start, edit.range.end);
            let inserted = edit.inserted_len() as isize;
            let removed = (end - start) as isize;

            if offset > end {
                // Wholly after this edit: shift by its net length change.
                result += inserted - removed;
            } else if offset == end {
                if start == end {
                    // A pure insertion exactly at this offset — the ambiguous case.
                    if bias == Bias::After {
                        result += inserted;
                    }
                } else {
                    // A deletion or replacement ending here; land after the new text.
                    result += inserted - removed;
                }
            } else if offset > start {
                // Strictly inside the replaced span.
                result += (start as isize + inserted) - offset as isize;
            }
            // offset <= start: this edit is entirely after us, nothing to do.
        }
        result.max(0) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::{Selection, Selections};

    fn apply(text: &str, edits: Vec<Edit>) -> (Buffer, Change) {
        let mut b = Buffer::from_text(text);
        let c = Change::apply(&mut b, edits);
        (b, c)
    }

    #[test]
    fn single_insert() {
        let (b, _) = apply("hello world", vec![Edit::insert(5, ",")]);
        assert_eq!(b.text(), "hello, world");
    }

    #[test]
    fn multi_cursor_insert_lands_at_every_cursor() {
        // Three cursors, one char each. If the later offsets were not shifted by
        // the earlier insertions, this would come out as "x_ax_bcx_" or worse.
        let (b, _) = apply(
            "abc",
            vec![
                Edit::insert(0, "x"),
                Edit::insert(1, "x"),
                Edit::insert(3, "x"),
            ],
        );
        assert_eq!(b.text(), "xaxbcx");
    }

    #[test]
    fn multi_cursor_edits_of_differing_widths_stay_aligned() {
        let (b, _) = apply(
            "one two three",
            vec![
                Edit::replace(0..3, "1"),
                Edit::replace(4..7, "22222"),
                Edit::replace(8..13, "3"),
            ],
        );
        assert_eq!(b.text(), "1 22222 3");
    }

    #[test]
    fn edits_may_be_supplied_out_of_order() {
        let (b, _) = apply(
            "abc",
            vec![
                Edit::insert(3, "z"),
                Edit::insert(0, "x"),
                Edit::insert(1, "y"),
            ],
        );
        assert_eq!(b.text(), "xaybcz");
    }

    #[test]
    fn undo_restores_the_original_exactly() {
        for (text, edits) in [
            ("hello", vec![Edit::insert(5, " world")]),
            ("hello world", vec![Edit::delete(5..11)]),
            (
                "one two three",
                vec![Edit::replace(0..3, "1"), Edit::replace(8..13, "333333")],
            ),
            ("héllo 🦀", vec![Edit::replace(6..7, "crab")]),
        ] {
            let mut b = Buffer::from_text(text);
            let change = Change::apply(&mut b, edits);
            assert_ne!(b.text(), text, "test case did not actually change anything");
            change.undo(&mut b);
            assert_eq!(b.text(), text, "undo did not restore {text:?}");
        }
    }

    #[test]
    fn undo_redo_cycles_are_stable() {
        let mut b = Buffer::from_text("abc");
        let change = Change::apply(
            &mut b,
            vec![
                Edit::insert(0, "x"),
                Edit::insert(1, "y"),
                Edit::insert(3, "z"),
            ],
        );
        let after = b.text();
        for _ in 0..5 {
            change.undo(&mut b);
            assert_eq!(b.text(), "abc");
            change.redo(&mut b);
            assert_eq!(b.text(), after);
        }
    }

    #[test]
    fn a_splice_is_one_atomic_replacement() {
        let mut b = Buffer::from_text("hello world");
        let len = b.len_chars();
        Change::apply(&mut b, vec![Splice::new(6, 5, "there").to_edit(len)]);
        assert_eq!(b.text(), "hello there");
        assert_eq!(Splice::new(6, 5, "there").len_delta(), 0);
        assert_eq!(Splice::new(0, 0, "🦀").len_delta(), 1, "chars, not bytes");
    }

    #[test]
    fn sequential_splices_address_what_the_previous_one_produced() {
        // The coordinate contract that separates Splice from Edit. In one
        // Change these two would share pre-change coordinates and overlap; in
        // sequence, each addresses the document the one before it left behind.
        let mut b = Buffer::from_text("abcdef");
        for splice in [Splice::new(0, 3, "X"), Splice::new(1, 3, "Y")] {
            let len = b.len_chars();
            Change::apply(&mut b, vec![splice.to_edit(len)]);
        }
        assert_eq!(b.text(), "XY");
    }

    #[test]
    fn a_malformed_splice_is_clamped_rather_than_trusted() {
        // Nothing upstream of a splice has to be honest — it comes off the
        // network. `pos + del` must not wrap, and the edit it becomes must not
        // address text that is not there.
        let mut b = Buffer::from_text("abc");
        let edit = Splice::new(2, usize::MAX, "Z").to_edit(b.len_chars());
        assert_eq!(edit.range, 2..3);
        Change::apply(&mut b, vec![edit]);
        assert_eq!(b.text(), "abZ");

        let edit = Splice::new(usize::MAX, 4, "!").to_edit(b.len_chars());
        assert_eq!(edit.range, 3..3, "a position past the end lands at the end");
        Change::apply(&mut b, vec![edit]);
        assert_eq!(b.text(), "abZ!");
    }

    #[test]
    fn an_out_of_range_edit_cannot_desynchronize_the_ones_after_it() {
        // The hazard this guards: `Buffer` clamps its own operations, so a bad
        // edit could never panic — but a running delta taken from the range it
        // *asked* for would misplace every later edit in the same change, and a
        // collaborative session turns that arithmetic into a backwards deletion
        // on every peer.
        let mut b = Buffer::from_text("abcdef");
        let change = Change::apply(
            &mut b,
            vec![Edit::replace(0..99, "X"), Edit::insert(99, "Y")],
        );
        assert_eq!(b.text(), "XY");
        // And the recorded inverse describes what actually happened, so undo
        // still restores the original exactly.
        change.undo(&mut b);
        assert_eq!(b.text(), "abcdef");
    }

    #[test]
    fn undo_and_redo_return_the_change_they_applied() {
        let mut b = Buffer::from_text("hello");
        let change = Change::apply(&mut b, vec![Edit::insert(5, " world")]);
        let undone = change.undo(&mut b);
        assert_eq!(undone.edits(), [Edit::delete(5..11)]);
        let redone = change.redo(&mut b);
        assert_eq!(redone.edits(), [Edit::insert(5, " world")]);
        assert_eq!(b.text(), "hello world");
    }

    #[test]
    fn map_offset_shifts_cursors_past_an_insertion() {
        let (_, c) = apply("abc", vec![Edit::insert(1, "XY")]);
        for bias in [Bias::Before, Bias::After] {
            assert_eq!(c.map_offset(0, bias), 0, "before the edit: unmoved");
            assert_eq!(c.map_offset(2, bias), 4, "after the edit: shifted by 2");
        }
    }

    #[test]
    fn bias_decides_which_side_of_an_insertion_a_cursor_lands_on() {
        let (_, c) = apply("abc", vec![Edit::insert(1, "XY")]);
        // "aXYbc" — offset 1 is the seam. The typist rides to 3; a bystander stays at 1.
        assert_eq!(c.map_offset(1, Bias::After), 3, "the caret that typed it");
        assert_eq!(c.map_offset(1, Bias::Before), 1, "a caret someone typed at");
    }

    #[test]
    fn map_offset_collapses_cursors_inside_deleted_text() {
        let (_, c) = apply("abcdef", vec![Edit::delete(1..5)]);
        for bias in [Bias::Before, Bias::After] {
            assert_eq!(c.map_offset(3, bias), 1, "inside the deletion: collapses");
            assert_eq!(c.map_offset(5, bias), 1, "at its trailing edge");
            assert_eq!(c.map_offset(6, bias), 2, "past it: shifted by 4");
            assert_eq!(c.map_offset(1, bias), 1, "at its leading edge: unmoved");
        }
    }

    #[test]
    fn typing_at_every_cursor_keeps_the_cursors_correct() {
        // The end-to-end multi-cursor loop: derive edits from selections, apply,
        // then rebase the selections through the change.
        let mut b = Buffer::from_text("aa\nbb\ncc");
        let sels = Selections::new(
            vec![
                Selection::cursor(0),
                Selection::cursor(3),
                Selection::cursor(6),
            ],
            0,
        );
        let edits = sels
            .iter()
            .map(|s| Edit::replace(s.range(), ">"))
            .collect::<Vec<_>>();
        let change = Change::apply(&mut b, edits);
        assert_eq!(b.text(), ">aa\n>bb\n>cc");

        // Bias::After with no manual fudge: each caret rides its own insertion.
        let moved: Vec<usize> = sels
            .iter()
            .map(|s| change.map_offset(s.head, Bias::After))
            .collect();
        assert_eq!(moved, vec![1, 5, 9]);
        for (offset, expected_line) in moved.iter().zip([0, 1, 2]) {
            assert_eq!(b.position_of(*offset).line, expected_line);
            assert_eq!(b.position_of(*offset).col, 1);
        }
    }
}
