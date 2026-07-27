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
    /// A collaborator's edit, replayed into this document. The view must *not*
    /// scroll the caret into view for one: the document moving under you
    /// because somebody else is typing is not a reason to be dragged somewhere
    /// you did not ask to go.
    Remote,
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
    /// Local changes awaiting publication to a collaborative document, oldest
    /// first. Empty and never appended to unless recording is on, so a
    /// standalone editor does not accumulate a list nobody drains.
    local_changes: Vec<Change>,
    recording: bool,
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
            local_changes: Vec::new(),
            recording: false,
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
        let (selections, applied) = self.history.undo(&mut self.buffer)?;
        self.record_all(&applied);
        Some(self.after_history(selections, EditKind::Undo))
    }

    /// Re-apply one undo group.
    pub fn redo(&mut self) -> Option<EditSummary> {
        let (selections, applied) = self.history.redo(&mut self.buffer)?;
        self.record_all(&applied);
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

    // ---- collaboration ----------------------------------------------------

    /// Start (or stop) recording local text changes for a collaborative
    /// session. Off by default.
    ///
    /// While on, every local mutation is queued for
    /// [`take_local_changes`](Self::take_local_changes) — typing, deletion,
    /// cut, paste, **and undo and redo**. Undo is on that list deliberately:
    /// to a shared document, reverting is not a rewind but another edit, and
    /// one that goes unpublished silently splits the session in two.
    ///
    /// Turning recording off discards anything still queued.
    pub fn record_local_changes(&mut self, on: bool) {
        self.recording = on;
        if !on {
            self.local_changes.clear();
        }
    }

    /// Whether any recorded local change is still waiting to be published.
    pub fn has_local_changes(&self) -> bool {
        !self.local_changes.is_empty()
    }

    /// Whether this model is recording local changes for a collaborative
    /// session.
    pub fn is_recording_local_changes(&self) -> bool {
        self.recording
    }

    /// Take everything recorded since the last call, oldest first.
    ///
    /// Hand all of it to the collaborative document, in this order, before
    /// applying anything that came back from the network — see
    /// [`apply_remote`](Self::apply_remote), where the order is not optional.
    pub fn take_local_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.local_changes)
    }

    fn record(&mut self, change: &Change) {
        if self.recording {
            self.local_changes.push(change.clone());
        }
    }

    fn record_all(&mut self, changes: &[Change]) {
        if self.recording {
            self.local_changes.extend_from_slice(changes);
        }
    }

    /// Apply one remote replacement as its own atomic change: buffer and
    /// highlighter, nothing else. No history (you cannot undo somebody else's
    /// typing), no recording (this edit came *from* the shared document), no
    /// version bump — the caller summarizes the whole batch at once.
    ///
    /// `edit` must already be clamped into the document; the collaborative path
    /// does that once, at the single point where a [`Splice`] becomes an
    /// [`Edit`], so the clamped range can also steer the selection rebasing.
    pub(crate) fn apply_edit(&mut self, edit: Edit) -> Change {
        let change = Change::apply(&mut self.buffer, vec![edit]);
        self.highlighter.apply_change(&self.buffer, &change);
        change
    }

    pub(crate) fn clear_history(&mut self) {
        self.history.clear();
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

        let after = Selections::new(
            new_carets.into_iter().map(Selection::cursor).collect(),
            primary,
        );
        Some(self.commit(kind, coalesce, edits, after))
    }

    /// The commit tail every text-changing operation shares: apply the edits as
    /// one atomic change, install the post-change selections, record history,
    /// resync the highlighter, and summarize — the four steps that must happen
    /// together, in this order (see the crate docs). Callers hand in `after` in
    /// *post-change* coordinates because only they know where their carets land:
    /// the cursor-edit pipeline above collapses to the end of each insertion,
    /// while the clipboard operations (cut's line-wise deletes, paste-above's
    /// preserved caret) place carets the running-delta rule cannot express.
    ///
    /// Resets coalescing to `None`; an operation that extends a run (`insert`,
    /// `backspace`) sets its kind again afterwards.
    pub(crate) fn commit(
        &mut self,
        kind: EditKind,
        coalesce: bool,
        edits: Vec<Edit>,
        after: Selections,
    ) -> EditSummary {
        let before = self.selections.clone();
        let lines_before = self.buffer.len_lines();
        let change = Change::apply(&mut self.buffer, edits);
        self.selections = after.clone();
        self.history.push(change.clone(), before, after, coalesce);
        self.highlighter.apply_change(&self.buffer, &change);
        self.coalesce = CoalesceKind::None;
        self.record(&change);
        self.summarize(&change, kind, lines_before)
    }

    fn summarize(&mut self, change: &Change, kind: EditKind, lines_before: usize) -> EditSummary {
        // `inverse` is recorded in post-change coordinates and spans exactly the
        // newly-inserted text (an empty span at the edit point for a pure
        // deletion), so its extent is the region that needs repainting.
        let (lo, hi) = change
            .inverse()
            .iter()
            .fold((usize::MAX, 0usize), |(lo, hi), e| {
                (lo.min(e.range.start), hi.max(e.range.end))
            });
        self.summarize_extent(lo, hi, kind, lines_before)
    }

    /// Summarize a post-change char span `lo..hi` as the lines needing repaint.
    ///
    /// An empty span is reported as an empty line range at `lo` rather than as
    /// the backwards range the fold below would otherwise produce — a change
    /// that touched nothing needs no repaint, and `changed_lines` is documented
    /// to be a safe over-approximation, which a reversed range is not.
    pub(crate) fn summarize_extent(
        &mut self,
        lo: usize,
        hi: usize,
        kind: EditKind,
        lines_before: usize,
    ) -> EditSummary {
        self.version += 1;
        let lines_after = self.buffer.len_lines();
        let line_count_changed = lines_after != lines_before;

        if hi < lo {
            let line = self.buffer.position_of(hi).line;
            return EditSummary {
                version: self.version,
                kind,
                changed_lines: line..line,
                line_count_changed,
            };
        }

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
