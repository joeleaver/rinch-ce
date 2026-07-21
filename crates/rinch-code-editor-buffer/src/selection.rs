use std::ops::Range;

/// One cursor, or one selected range when `anchor != head`.
///
/// `head` is the moving end (where the caret is drawn); `anchor` is the fixed end.
/// Keeping them distinct rather than storing a sorted range is what makes
/// shift-arrow extension behave correctly when the selection crosses its origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    /// Remembered column for vertical motion. Moving down through a short line
    /// and back out must return to the original column, so the desired column
    /// survives lines that cannot satisfy it.
    pub goal_col: Option<usize>,
}

impl Selection {
    pub fn cursor(offset: usize) -> Self {
        Self {
            anchor: offset,
            head: offset,
            goal_col: None,
        }
    }

    pub fn new(anchor: usize, head: usize) -> Self {
        Self {
            anchor,
            head,
            goal_col: None,
        }
    }

    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    pub fn range(&self) -> Range<usize> {
        self.start()..self.end()
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// True when the caret sits before the anchor (a backwards drag).
    pub fn is_reversed(&self) -> bool {
        self.head < self.anchor
    }
}

/// A non-empty, sorted, non-overlapping set of selections, with one marked primary.
///
/// The invariant is enforced on construction and after every mutation. Overlapping
/// cursors are the classic multi-cursor bug: two carets that drift into the same
/// spot must become one, or every subsequent edit is applied twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selections {
    items: Vec<Selection>,
    primary: usize,
}

impl Default for Selections {
    fn default() -> Self {
        Self::single(Selection::cursor(0))
    }
}

impl Selections {
    pub fn single(sel: Selection) -> Self {
        Self {
            items: vec![sel],
            primary: 0,
        }
    }

    /// Build from an arbitrary list, normalizing to the invariant.
    /// Panics if `items` is empty — a buffer always has at least one cursor.
    pub fn new(items: Vec<Selection>, primary: usize) -> Self {
        assert!(!items.is_empty(), "a buffer always has at least one cursor");
        let mut s = Self { items, primary };
        s.normalize();
        s
    }

    pub fn iter(&self) -> impl Iterator<Item = &Selection> {
        self.items.iter()
    }

    pub fn as_slice(&self) -> &[Selection] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn primary(&self) -> &Selection {
        &self.items[self.primary]
    }

    pub fn primary_index(&self) -> usize {
        self.primary
    }

    /// Replace every selection, re-normalizing.
    pub fn set(&mut self, items: Vec<Selection>, primary: usize) {
        assert!(!items.is_empty(), "a buffer always has at least one cursor");
        self.items = items;
        self.primary = primary;
        self.normalize();
    }

    /// Add a cursor (Ctrl-click / Ctrl-D). It becomes primary; if it collapses
    /// into an existing selection, that merged selection becomes primary instead.
    pub fn push(&mut self, sel: Selection) {
        self.items.push(sel);
        self.primary = self.items.len() - 1;
        self.normalize();
    }

    /// Collapse back to just the primary selection (Escape).
    pub fn collapse_to_primary(&mut self) {
        let p = self.items[self.primary];
        self.items = vec![p];
        self.primary = 0;
    }

    /// Sort by start offset and merge any selections that overlap or touch,
    /// keeping the primary pointed at whichever merged selection absorbed it.
    fn normalize(&mut self) {
        // Tag each selection with whether it is the primary, so the marker
        // survives sorting and merging.
        let primary_marker = self.primary;
        let mut tagged: Vec<(Selection, bool)> = self
            .items
            .drain(..)
            .enumerate()
            .map(|(i, s)| (s, i == primary_marker))
            .collect();
        tagged.sort_by_key(|(s, _)| (s.start(), s.end()));

        let mut merged: Vec<(Selection, bool)> = Vec::with_capacity(tagged.len());
        for (sel, is_primary) in tagged {
            match merged.last_mut() {
                // Touching counts as overlapping: a selection ending at 5 and one
                // starting at 5 describe one contiguous region, and leaving them
                // separate would apply an edit at offset 5 twice.
                Some((prev, prev_primary)) if prev.end() >= sel.start() => {
                    let start = prev.start().min(sel.start());
                    let end = prev.end().max(sel.end());
                    // Preserve the direction of the selection that was already there.
                    *prev = if prev.is_reversed() {
                        Selection::new(end, start)
                    } else {
                        Selection::new(start, end)
                    };
                    *prev_primary = *prev_primary || is_primary;
                }
                _ => merged.push((sel, is_primary)),
            }
        }

        self.primary = merged.iter().position(|(_, p)| *p).unwrap_or(0);
        self.items = merged.into_iter().map(|(s, _)| s).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coincident_cursors_merge_into_one() {
        let s = Selections::new(
            vec![
                Selection::cursor(3),
                Selection::cursor(3),
                Selection::cursor(7),
            ],
            0,
        );
        assert_eq!(s.len(), 2);
        assert_eq!(s.as_slice()[0], Selection::cursor(3));
        assert_eq!(s.as_slice()[1], Selection::cursor(7));
    }

    #[test]
    fn distinct_cursors_are_left_alone() {
        let s = Selections::new(vec![Selection::cursor(3), Selection::cursor(5)], 0);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn overlapping_and_touching_ranges_merge() {
        let overlapping = Selections::new(vec![Selection::new(0, 5), Selection::new(3, 9)], 0);
        assert_eq!(overlapping.len(), 1);
        assert_eq!(overlapping.as_slice()[0].range(), 0..9);

        let touching = Selections::new(vec![Selection::new(0, 5), Selection::new(5, 9)], 0);
        assert_eq!(touching.len(), 1, "abutting ranges describe one region");
        assert_eq!(touching.as_slice()[0].range(), 0..9);
    }

    #[test]
    fn selections_are_sorted_regardless_of_insertion_order() {
        let s = Selections::new(
            vec![
                Selection::cursor(9),
                Selection::cursor(1),
                Selection::cursor(5),
            ],
            0,
        );
        let starts: Vec<_> = s.iter().map(|x| x.start()).collect();
        assert_eq!(starts, vec![1, 5, 9]);
    }

    #[test]
    fn primary_follows_its_selection_through_sorting() {
        // Primary is the cursor at offset 9, which sorts to last.
        let s = Selections::new(
            vec![
                Selection::cursor(9),
                Selection::cursor(1),
                Selection::cursor(5),
            ],
            0,
        );
        assert_eq!(*s.primary(), Selection::cursor(9));
        assert_eq!(s.primary_index(), 2);
    }

    #[test]
    fn primary_survives_being_absorbed_by_a_merge() {
        // The primary (index 1) is swallowed by the range it overlaps.
        let s = Selections::new(vec![Selection::new(0, 6), Selection::new(4, 9)], 1);
        assert_eq!(s.len(), 1);
        assert_eq!(s.primary().range(), 0..9);
    }

    #[test]
    fn merging_preserves_direction_of_the_earlier_selection() {
        let s = Selections::new(vec![Selection::new(5, 0), Selection::new(3, 9)], 0);
        assert_eq!(s.len(), 1);
        assert!(s.as_slice()[0].is_reversed());
        assert_eq!(s.as_slice()[0].range(), 0..9);
    }

    #[test]
    fn collapse_keeps_only_the_primary() {
        let mut s = Selections::new(
            vec![
                Selection::cursor(1),
                Selection::cursor(5),
                Selection::cursor(9),
            ],
            1,
        );
        s.collapse_to_primary();
        assert_eq!(s.len(), 1);
        assert_eq!(*s.primary(), Selection::cursor(5));
    }
}
