use std::ops::Range;

use rinch_code_editor_buffer::{Buffer, Change, Edit, History, Selection, Selections};
use rinch_code_editor_syntax::{Highlighter, LineTokens, Theme};

use crate::cursor::{self, Motion};
use crate::geometry::Grid;
use crate::graphemes;

/// What produced an [`EditSummary`]. The view uses this to decide, for instance,
/// whether to scroll the primary caret into view (yes on an edit, yes on undo).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    Insert,
    Delete,
    Undo,
    Redo,
}

/// The open undo group's shape, so the model can decide coalescing without a
/// clock. A run of single-character typing folds into one group; a run of
/// deletes folds into another; anything else — a paste, a newline, a caret move
/// — starts fresh. Kept private: callers express *intent* (`insert`, `backspace`)
/// and the model derives the grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoalesceKind {
    /// No group is open to extend; the next edit starts a new one.
    None,
    Typing,
    Deleting,
}

/// What a single model operation changed, handed back so the view can repaint the
/// minimum and keep its virtualization honest.
///
/// `changed_lines` is a *post-change* model-line range and is always a safe
/// over-approximation: repainting exactly it is correct, repainting more is only
/// wasteful. When `line_count_changed` is true the line total moved, so every
/// line from `changed_lines.start` to the end may have shifted and the range is
/// widened to the bottom of the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditSummary {
    pub version: u64,
    pub kind: EditKind,
    pub changed_lines: Range<usize>,
    pub line_count_changed: bool,
}

/// The whole document: text, cursors, undo history, and highlighting behind one
/// edit API. See the crate docs for why the four move together.
pub struct EditorModel {
    buffer: Buffer,
    selections: Selections,
    history: History,
    highlighter: Box<dyn Highlighter>,
    /// Bumped on every operation that changes the text. The view keys its
    /// repaint on this, so a no-op edit (backspace at the start of the document)
    /// deliberately does not bump it.
    version: u64,
    coalesce: CoalesceKind,
}

impl EditorModel {
    /// An empty document highlighted by `highlighter`.
    pub fn new(highlighter: Box<dyn Highlighter>) -> Self {
        Self::from_buffer(Buffer::new(), highlighter)
    }

    /// A document initialized with `text`.
    pub fn from_text(text: &str, highlighter: Box<dyn Highlighter>) -> Self {
        Self::from_buffer(Buffer::from_text(text), highlighter)
    }

    fn from_buffer(buffer: Buffer, mut highlighter: Box<dyn Highlighter>) -> Self {
        // Prime the highlighter against the starting text; without this the first
        // `tokens()` call would parse an empty document.
        highlighter.reset(&buffer);
        Self {
            buffer,
            selections: Selections::default(),
            history: History::default(),
            highlighter,
            version: 0,
            coalesce: CoalesceKind::None,
        }
    }

    // ---- reads ------------------------------------------------------------

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    pub fn selections(&self) -> &Selections {
        &self.selections
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn len_lines(&self) -> usize {
        self.buffer.len_lines()
    }

    pub fn len_chars(&self) -> usize {
        self.buffer.len_chars()
    }

    pub fn text(&self) -> String {
        self.buffer.text()
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    pub fn theme(&self) -> &Theme {
        self.highlighter.theme()
    }

    /// Tokens for `lines`, appended to `out`. Mirrors [`Highlighter::tokens`]
    /// exactly, including that `out` is appended to (not cleared) and that lines
    /// past the end of the document contribute nothing, so `out` may gain fewer
    /// than `lines.len()` entries.
    pub fn tokens(&mut self, lines: Range<usize>, out: &mut Vec<LineTokens>) {
        self.highlighter.tokens(&self.buffer, lines, out);
    }

    /// Tokens for a single line.
    pub fn line_tokens(&mut self, line: usize) -> LineTokens {
        self.highlighter.line_tokens(&self.buffer, line)
    }

    // ---- selection (no text change) --------------------------------------

    /// Replace the selection set. This is a caret move, so it closes any open
    /// undo group: text typed after moving the caret is its own step.
    pub fn set_selections(&mut self, selections: Selections) {
        self.selections = selections;
        self.coalesce = CoalesceKind::None;
    }

    /// Collapse to a single cursor at `offset` (clamped into the document).
    pub fn set_cursor(&mut self, offset: usize) {
        let offset = offset.min(self.buffer.len_chars());
        self.set_selections(Selections::single(Selection::cursor(offset)));
    }

    // ---- edits ------------------------------------------------------------

    /// Insert `text` at every cursor, replacing any non-empty selection. A single
    /// non-newline character extends the current typing run so a word undoes at
    /// once; anything longer (a paste) or a newline is its own undo step.
    pub fn insert(&mut self, text: &str) -> Option<EditSummary> {
        if text.is_empty() {
            return None;
        }
        let is_typing = is_single_grapheme_typing(text);
        let coalesce = is_typing && self.coalesce == CoalesceKind::Typing;

        let summary = self.apply_cursor_edits(EditKind::Insert, coalesce, |sel, _buffer| {
            Some((sel.range(), text.to_string()))
        })?;

        self.coalesce = if is_typing {
            CoalesceKind::Typing
        } else {
            CoalesceKind::None
        };
        Some(summary)
    }

    /// Delete the selection at each cursor, or — when the cursor is empty — the
    /// grapheme before it. Consecutive backspaces coalesce into one undo step.
    /// Stepping by grapheme means a `\r\n` or a base-plus-combining-mark is
    /// removed as the one character the user sees, not as a fragment of it.
    pub fn backspace(&mut self) -> Option<EditSummary> {
        let coalesce = self.coalesce == CoalesceKind::Deleting;
        let summary = self.apply_cursor_edits(EditKind::Delete, coalesce, |sel, buffer| {
            if !sel.is_empty() {
                Some((sel.range(), String::new()))
            } else {
                let prev = graphemes::prev_boundary(buffer, sel.head);
                (prev < sel.head).then(|| (prev..sel.head, String::new()))
            }
        })?;
        self.coalesce = CoalesceKind::Deleting;
        Some(summary)
    }

    /// Delete the selection at each cursor, or — when the cursor is empty — the
    /// grapheme after it. Consecutive forward-deletes coalesce.
    pub fn delete_forward(&mut self) -> Option<EditSummary> {
        let coalesce = self.coalesce == CoalesceKind::Deleting;
        let summary = self.apply_cursor_edits(EditKind::Delete, coalesce, |sel, buffer| {
            if !sel.is_empty() {
                Some((sel.range(), String::new()))
            } else {
                let next = graphemes::next_boundary(buffer, sel.head);
                (next > sel.head).then(|| (sel.head..next, String::new()))
            }
        })?;
        self.coalesce = CoalesceKind::Deleting;
        Some(summary)
    }

    // ---- cursor motion (no text change) ----------------------------------

    /// Move every cursor by `motion`. With `extend` the selection grows (shift
    /// held); without it each selection collapses onto its new caret. `grid`
    /// supplies the visual-column geometry that vertical motion needs to hold its
    /// on-screen column; other motions ignore it. Like any caret move, this ends
    /// the open undo group.
    pub fn move_cursors(&mut self, grid: &Grid, motion: Motion, extend: bool) {
        let moved = cursor::move_cursors(&self.buffer, grid, &self.selections, motion, extend);
        self.set_selections(moved);
    }

    // ---- history ----------------------------------------------------------

    /// Revert one undo group, restoring both the text and the selections that
    /// were active before it.
    pub fn undo(&mut self) -> Option<EditSummary> {
        let selections = self.history.undo(&mut self.buffer)?;
        Some(self.after_history(selections, EditKind::Undo))
    }

    /// Re-apply one undo group.
    pub fn redo(&mut self) -> Option<EditSummary> {
        let selections = self.history.redo(&mut self.buffer)?;
        Some(self.after_history(selections, EditKind::Redo))
    }

    fn after_history(&mut self, selections: Selections, kind: EditKind) -> EditSummary {
        self.selections = selections;
        // History mutates the buffer directly rather than through our edit path,
        // so the highlighter never saw the reverse edits. A full reparse is the
        // correct, if coarse, way to resync; feeding the inverse Change to the
        // highlighter incrementally is a later optimization, tracked separately.
        self.highlighter.reset(&self.buffer);
        self.coalesce = CoalesceKind::None;
        self.version += 1;
        EditSummary {
            version: self.version,
            kind,
            // Conservative: an undo can touch anywhere, so repaint everything.
            changed_lines: 0..self.buffer.len_lines(),
            line_count_changed: true,
        }
    }

    // ---- the shared edit pipeline ----------------------------------------

    /// The one place edits become buffer mutations. `derive` turns each selection
    /// into an optional `(range_to_replace, replacement_text)` in *pre-change*
    /// coordinates; a `None` means that cursor contributes no edit (e.g. a
    /// backspace at offset 0) but its caret still rides earlier edits.
    ///
    /// Returns `None` — bumping nothing — when no selection produced an edit.
    fn apply_cursor_edits<F>(
        &mut self,
        kind: EditKind,
        coalesce: bool,
        mut derive: F,
    ) -> Option<EditSummary>
    where
        F: FnMut(&Selection, &Buffer) -> Option<(Range<usize>, String)>,
    {
        let before = self.selections.clone();
        let primary = before.primary_index();

        let mut edits: Vec<Edit> = Vec::with_capacity(before.len());
        let mut new_carets: Vec<usize> = Vec::with_capacity(before.len());
        // Walk the selections in their sorted order, carrying the net offset
        // shift from every edit already accounted for — the same running-delta
        // discipline `Change::apply` uses, so the caret positions we predict here
        // match the buffer the change is about to produce. Crucially, the new
        // caret is computed as `replace_start + inserted_len`: typing over a
        // selection collapses the caret to the end of the inserted text rather
        // than re-selecting it, which is what naively rebasing both ends would do.
        let mut delta: isize = 0;
        for sel in before.iter() {
            match derive(sel, &self.buffer) {
                Some((range, text)) => {
                    let inserted = text.chars().count();
                    let removed = range.end - range.start;
                    let caret = (range.start as isize + delta) as usize + inserted;
                    new_carets.push(caret);
                    delta += inserted as isize - removed as isize;
                    edits.push(Edit::replace(range, text));
                }
                None => {
                    new_carets.push((sel.head as isize + delta) as usize);
                }
            }
        }

        if edits.is_empty() {
            return None;
        }

        let lines_before = self.buffer.len_lines();
        let change = Change::apply(&mut self.buffer, edits);
        let after = Selections::new(
            new_carets.into_iter().map(Selection::cursor).collect(),
            primary,
        );
        self.selections = after.clone();
        self.history.push(change.clone(), before, after, coalesce);
        self.highlighter.apply_change(&self.buffer, &change);

        Some(self.summarize(&change, kind, lines_before))
    }

    fn summarize(&mut self, change: &Change, kind: EditKind, lines_before: usize) -> EditSummary {
        self.version += 1;
        let lines_after = self.buffer.len_lines();
        let line_count_changed = lines_after != lines_before;

        // `inverse` is recorded in post-change coordinates and spans exactly the
        // newly-inserted text (an empty span at the edit point for a pure
        // deletion), so its extent is the region that needs repainting.
        let (lo, hi) = change
            .inverse()
            .iter()
            .fold((usize::MAX, 0usize), |(lo, hi), e| {
                (lo.min(e.range.start), hi.max(e.range.end))
            });
        let first = self.buffer.position_of(lo).line;
        let last = if line_count_changed {
            lines_after
        } else {
            self.buffer.position_of(hi).line + 1
        };

        EditSummary {
            version: self.version,
            kind,
            changed_lines: first..last,
            line_count_changed,
        }
    }
}

/// True for exactly one character that is not a line break — the only shape that
/// extends a typing run. A pasted single newline, or any multi-character string,
/// starts its own undo step.
fn is_single_grapheme_typing(text: &str) -> bool {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => c != '\n' && c != '\r',
        _ => false,
    }
}
