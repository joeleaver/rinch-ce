use crate::buffer::Buffer;
use crate::edit::Change;
use crate::selection::Selections;

/// One undo step. Consecutive keystrokes are coalesced into a single group so
/// that Ctrl+Z undoes a word, not a letter — but the grouping decision is the
/// caller's, since this crate has no clock (see the crate docs).
#[derive(Debug, Clone)]
struct Group {
    changes: Vec<Change>,
    before: Selections,
    after: Selections,
}

/// Undo/redo stacks.
///
/// Selections are restored alongside the text: undoing a deletion should leave
/// the text re-selected, which is what makes undo feel like it rewinds the
/// editor rather than just the document.
#[derive(Debug, Clone)]
pub struct History {
    undo_stack: Vec<Group>,
    redo_stack: Vec<Group>,
    limit: usize,
}

impl Default for History {
    fn default() -> Self {
        Self::new(500)
    }
}

impl History {
    pub fn new(limit: usize) -> Self {
        Self {
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            limit: limit.max(1),
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// Record an applied change.
    ///
    /// `coalesce` folds this change into the previous undo step instead of
    /// starting a new one. The caller sets it for a run of plain typing and
    /// clears it on anything that should be its own step — a caret jump, a paste,
    /// a newline, or simply too long a pause.
    pub fn push(&mut self, change: Change, before: Selections, after: Selections, coalesce: bool) {
        // Any new edit invalidates the redo branch.
        self.redo_stack.clear();

        match self.undo_stack.last_mut() {
            Some(group) if coalesce => {
                group.changes.push(change);
                group.after = after;
            }
            _ => {
                self.undo_stack.push(Group {
                    changes: vec![change],
                    before,
                    after,
                });
                if self.undo_stack.len() > self.limit {
                    self.undo_stack.remove(0);
                }
            }
        }
    }

    /// Revert one group. Returns the selections to restore and the changes
    /// that were applied to the buffer, in application order — reverting is
    /// itself an edit, and a collaborative session must publish it like any
    /// other.
    pub fn undo(&mut self, buffer: &mut Buffer) -> Option<(Selections, Vec<Change>)> {
        let group = self.undo_stack.pop()?;
        // Reverse order: the last change applied is the first undone.
        let applied = group.changes.iter().rev().map(|c| c.undo(buffer)).collect();
        let selections = group.before.clone();
        self.redo_stack.push(group);
        Some((selections, applied))
    }

    /// Re-apply one group. Returns the selections to restore and the changes
    /// that were applied — see [`History::undo`].
    pub fn redo(&mut self, buffer: &mut Buffer) -> Option<(Selections, Vec<Change>)> {
        let group = self.redo_stack.pop()?;
        let applied = group.changes.iter().map(|c| c.redo(buffer)).collect();
        let selections = group.after.clone();
        self.undo_stack.push(group);
        Some((selections, applied))
    }

    /// Drop both stacks.
    ///
    /// Undo is only meaningful while the recorded edits still describe the
    /// document in front of the user. When that stops being true — a
    /// collaborative session resynchronizing its whole text, say — forgetting
    /// is the honest answer; replaying stale edits would corrupt the buffer.
    pub fn clear(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Edit;
    use crate::selection::Selection;

    fn sel(offset: usize) -> Selections {
        Selections::single(Selection::cursor(offset))
    }

    fn type_char(buffer: &mut Buffer, history: &mut History, at: usize, ch: &str, coalesce: bool) {
        let before = sel(at);
        let change = Change::apply(buffer, vec![Edit::insert(at, ch)]);
        history.push(change, before, sel(at + ch.chars().count()), coalesce);
    }

    #[test]
    fn coalesced_typing_undoes_as_one_step() {
        let mut b = Buffer::new();
        let mut h = History::default();
        for (i, ch) in ["h", "e", "l", "l", "o"].iter().enumerate() {
            type_char(&mut b, &mut h, i, ch, i > 0);
        }
        assert_eq!(b.text(), "hello");

        assert!(h.undo(&mut b).is_some());
        assert_eq!(b.text(), "", "the whole run should undo at once");
        assert!(!h.can_undo());
    }

    #[test]
    fn uncoalesced_edits_undo_one_at_a_time() {
        let mut b = Buffer::new();
        let mut h = History::default();
        for (i, ch) in ["a", "b", "c"].iter().enumerate() {
            type_char(&mut b, &mut h, i, ch, false);
        }
        assert_eq!(b.text(), "abc");
        h.undo(&mut b);
        assert_eq!(b.text(), "ab");
        h.undo(&mut b);
        assert_eq!(b.text(), "a");
        h.undo(&mut b);
        assert_eq!(b.text(), "");
        assert!(h.undo(&mut b).is_none());
    }

    #[test]
    fn redo_replays_a_coalesced_group() {
        let mut b = Buffer::new();
        let mut h = History::default();
        for (i, ch) in ["a", "b", "c"].iter().enumerate() {
            type_char(&mut b, &mut h, i, ch, i > 0);
        }
        h.undo(&mut b);
        assert_eq!(b.text(), "");
        assert!(h.can_redo());
        h.redo(&mut b);
        assert_eq!(b.text(), "abc");
        assert!(!h.can_redo());
    }

    #[test]
    fn selections_are_restored_by_undo_and_redo() {
        let mut b = Buffer::from_text("hello");
        let mut h = History::default();
        let before = Selections::single(Selection::new(0, 5));
        let change = Change::apply(&mut b, vec![Edit::replace(0..5, "x")]);
        h.push(change, before.clone(), sel(1), false);

        assert_eq!(
            h.undo(&mut b).unwrap().0,
            before,
            "undo re-selects the old text"
        );
        assert_eq!(b.text(), "hello");
        assert_eq!(h.redo(&mut b).unwrap().0, sel(1));
        assert_eq!(b.text(), "x");
    }

    #[test]
    fn undo_and_redo_report_the_changes_they_applied() {
        // A collaborative session republishes these, so they must describe the
        // buffer mutation exactly — in the order it happened.
        let mut b = Buffer::from_text("hello");
        let mut h = History::default();
        let before = sel(5);
        let change = Change::apply(&mut b, vec![Edit::insert(5, " world")]);
        h.push(change, before, sel(11), false);

        let (_, applied) = h.undo(&mut b).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].edits(), [Edit::delete(5..11)]);
        // Replaying the reported change on a fresh copy reproduces the undo.
        let mut replay = Buffer::from_text("hello world");
        Change::apply(&mut replay, applied[0].edits().to_vec());
        assert_eq!(replay.text(), b.text());

        let (_, applied) = h.redo(&mut b).unwrap();
        assert_eq!(applied[0].edits(), [Edit::insert(5, " world")]);
        Change::apply(&mut replay, applied[0].edits().to_vec());
        assert_eq!(replay.text(), b.text());
    }

    #[test]
    fn a_coalesced_group_reports_its_changes_in_application_order() {
        let mut b = Buffer::new();
        let mut h = History::default();
        for (i, ch) in ["a", "b", "c"].iter().enumerate() {
            type_char(&mut b, &mut h, i, ch, i > 0);
        }
        let (_, applied) = h.undo(&mut b).unwrap();
        assert_eq!(b.text(), "");
        // Three keystrokes coalesced into one group undo as three deletions,
        // last-typed first: replaying them in this order must reproduce it.
        let mut replay = Buffer::from_text("abc");
        for change in &applied {
            Change::apply(&mut replay, change.edits().to_vec());
        }
        assert_eq!(replay.text(), "");
        assert_eq!(
            applied
                .iter()
                .map(|c| c.edits()[0].range.clone())
                .collect::<Vec<_>>(),
            vec![2..3, 1..2, 0..1]
        );
    }

    #[test]
    fn clear_drops_both_stacks() {
        let mut b = Buffer::new();
        let mut h = History::default();
        type_char(&mut b, &mut h, 0, "a", false);
        h.undo(&mut b);
        assert!(h.can_redo());
        type_char(&mut b, &mut h, 0, "b", false);
        h.clear();
        assert!(!h.can_undo());
        assert!(!h.can_redo());
        assert_eq!(b.text(), "b", "clearing forgets history, not text");
    }

    #[test]
    fn a_new_edit_discards_the_redo_branch() {
        let mut b = Buffer::new();
        let mut h = History::default();
        type_char(&mut b, &mut h, 0, "a", false);
        h.undo(&mut b);
        assert!(h.can_redo());
        type_char(&mut b, &mut h, 0, "z", false);
        assert!(!h.can_redo(), "branching must drop the stale redo path");
        assert_eq!(b.text(), "z");
    }

    #[test]
    fn multi_cursor_edits_undo_as_one_step() {
        let mut b = Buffer::from_text("aa\nbb\ncc");
        let mut h = History::default();
        let before = Selections::new(
            vec![
                Selection::cursor(0),
                Selection::cursor(3),
                Selection::cursor(6),
            ],
            0,
        );
        let edits = before
            .iter()
            .map(|s| Edit::insert(s.head, ">"))
            .collect::<Vec<_>>();
        let change = Change::apply(&mut b, edits);
        h.push(change, before, sel(1), false);
        assert_eq!(b.text(), ">aa\n>bb\n>cc");

        h.undo(&mut b);
        assert_eq!(b.text(), "aa\nbb\ncc", "all three carets undo together");
    }

    #[test]
    fn the_stack_is_bounded() {
        let mut b = Buffer::new();
        let mut h = History::new(3);
        for i in 0..10 {
            type_char(&mut b, &mut h, i, "x", false);
        }
        let mut steps = 0;
        while h.undo(&mut b).is_some() {
            steps += 1;
        }
        assert_eq!(steps, 3, "only the last `limit` steps are retained");
    }
}
