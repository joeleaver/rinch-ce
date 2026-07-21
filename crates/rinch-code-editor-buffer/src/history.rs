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

    /// Revert one group. Returns the selections to restore.
    pub fn undo(&mut self, buffer: &mut Buffer) -> Option<Selections> {
        let group = self.undo_stack.pop()?;
        // Reverse order: the last change applied is the first undone.
        for change in group.changes.iter().rev() {
            change.undo(buffer);
        }
        let selections = group.before.clone();
        self.redo_stack.push(group);
        Some(selections)
    }

    /// Re-apply one group. Returns the selections to restore.
    pub fn redo(&mut self, buffer: &mut Buffer) -> Option<Selections> {
        let group = self.redo_stack.pop()?;
        for change in &group.changes {
            change.redo(buffer);
        }
        let selections = group.after.clone();
        self.undo_stack.push(group);
        Some(selections)
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
            h.undo(&mut b).unwrap(),
            before,
            "undo re-selects the old text"
        );
        assert_eq!(b.text(), "hello");
        assert_eq!(h.redo(&mut b).unwrap(), sel(1));
        assert_eq!(b.text(), "x");
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
