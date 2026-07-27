//! Clipboard semantics: what copy takes, how cut deletes, where paste lands.
//!
//! A direct port of Monaco's clipboard behavior, from the current vscode
//! source — `getPlainTextToCopy` (viewModelImpl.ts) and `getDataToCopy`
//! (clipboardUtils.ts) for copy, `DeleteOperations.cut`
//! (cursorDeleteOperations.ts) for cut, and `PasteOperation.getEdits`
//! (cursorTypeEditOperations.ts) for paste. Monaco's `emptySelectionClipboard`
//! option (default true) is modeled as always on, and `multiCursorPaste` as its
//! default `'spread'`.
//!
//! The system clipboard itself is deliberately absent: this module computes the
//! payload text and applies the edits, and the view layer moves strings in and
//! out of the real clipboard. That keeps every rule here — the full-line copy,
//! the three cut cases, the paste distribution — unit-testable without a GUI,
//! like the rest of the crate.
//!
//! The one Monaco piece that lives in the view layer instead is the in-memory
//! metadata store (`InMemoryClipboardMetadataManager`): whether the stored
//! [`ClipboardPayload`] still matches the clipboard's text is a question about
//! the *system* clipboard, so the check belongs next to it.

use std::ops::Range;

use rinch_code_editor_buffer::{Edit, Selection, Selections};

use crate::model::{EditKind, EditSummary, EditorModel};

/// What one copy (or the copy half of a cut) produces. `text` is what goes on
/// the system clipboard; the other two fields are the metadata Monaco stows
/// beside it, consulted on paste only while the clipboard still holds exactly
/// `text` (the view layer's equality check).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardPayload {
    /// The clipboard text: the single piece, or the pieces joined with `\n`.
    pub text: String,
    /// True only for a *single empty* cursor — the whole-line copy. A paste of
    /// this payload inserts the line above the current one (Monaco's
    /// `isFromEmptySelection`).
    pub is_from_empty_selection: bool,
    /// The individual pieces when there were two or more, in selection order —
    /// pasting onto the same number of cursors hands one piece to each
    /// (Monaco's `multicursorText`).
    pub multicursor_text: Option<Vec<String>>,
}

impl EditorModel {
    /// The clipboard payload for the current selections — Monaco's
    /// `getPlainTextToCopy` with `emptySelectionClipboard` on. An empty
    /// selection contributes its whole line plus a trailing newline,
    /// deduplicated when the previous selection already started on that line;
    /// a non-empty selection contributes exactly its text.
    ///
    /// `force_crlf` is Monaco's third parameter, passed `isWindows` there: the
    /// system clipboard convention on Windows is `\r\n`, so the view passes
    /// true on that platform and every newline in the payload — inside pieces,
    /// trailing a whole-line copy, joining pieces — becomes `\r\n`. The model
    /// itself stays `\n`; pasting a CRLF payload back normalizes on insert.
    pub fn clipboard_payload(&self, force_crlf: bool) -> ClipboardPayload {
        let newline = if force_crlf { "\r\n" } else { "\n" };
        let buffer = self.buffer();
        let sels = self.selections().as_slice();
        let has_empty = sels.iter().any(|s| s.is_empty());

        // A non-empty selection's text, in the clipboard's EOL convention —
        // Monaco's getValueInRange with EndOfLinePreference::CRLF when forced.
        let piece_of = |sel: &Selection| {
            let text = buffer.slice(sel.range());
            if force_crlf {
                text.replace('\n', "\r\n")
            } else {
                text
            }
        };

        let mut pieces: Vec<String> = Vec::new();
        if has_empty {
            // Mixed walk: full lines for empty selections (skipping a line the
            // previous selection — empty or not — already started on), exact
            // text for the rest.
            let mut prev_line: Option<usize> = None;
            for sel in sels {
                let line = buffer.position_of(sel.start()).line;
                if sel.is_empty() {
                    if prev_line != Some(line) {
                        pieces.push(format!("{}{}", buffer.line_text(line), newline));
                    }
                } else {
                    pieces.push(piece_of(sel));
                }
                prev_line = Some(line);
            }
        } else {
            for sel in sels {
                pieces.push(piece_of(sel));
            }
        }

        // Monaco flags a from-empty-selection copy only for a single cursor;
        // several empty cursors copy their lines but paste as plain text.
        let is_from_empty_selection = sels.len() == 1 && sels[0].is_empty();
        if pieces.len() == 1 {
            ClipboardPayload {
                text: pieces.pop().expect("one piece"),
                is_from_empty_selection,
                multicursor_text: None,
            }
        } else {
            ClipboardPayload {
                text: pieces.join(newline),
                is_from_empty_selection,
                multicursor_text: Some(pieces),
            }
        }
    }

    /// The delete half of a cut (the copy half is [`Self::clipboard_payload`],
    /// taken first). A non-empty selection deletes exactly itself; an empty
    /// cursor deletes its whole line, Monaco's three ways:
    ///
    /// - a middle line goes from its start through the next line's start
    ///   (content plus terminator), the caret landing at column 0 of the line
    ///   that moves up;
    /// - the last line of a multi-line document goes from the previous line's
    ///   end through its own end (the preceding terminator plus the content),
    ///   the caret landing at the new last line's end — unless an earlier
    ///   cursor's cut already ended on this line, which reroutes to:
    /// - a single (or already-touched last) line just clears its content.
    ///
    /// When a cursor's range overlaps one already emitted — two cursors on the
    /// same line — the later cursor's delete is dropped and its caret merged
    /// into the earlier one's landing spot, which is Monaco's loser-cursor
    /// resolution by construction. Returns `None` when nothing gets deleted
    /// (an empty cursor in an empty document); the payload is still valid to
    /// copy in that case, as in Monaco.
    pub fn cut(&mut self) -> Option<EditSummary> {
        let before = self.selections().clone();
        let buffer = self.buffer();
        let last_line = buffer.len_lines() - 1;

        let mut edits: Vec<Edit> = Vec::new();
        // Pre-change caret targets, one per cursor, mapped after the walk.
        let mut anchors: Vec<usize> = Vec::with_capacity(before.len());
        let mut last_emitted: Option<Range<usize>> = None;
        // Monaco's `lastCutRange` guard: tracks where the previous *empty*
        // cursor's cut ended, so a second cursor on the last line clears the
        // line instead of deleting the preceding terminator again.
        let mut last_cut_end_line: Option<usize> = None;

        for sel in before.iter() {
            let range = if sel.is_empty() {
                let line = buffer.position_of(sel.head).line;
                let r = if line < last_line {
                    buffer.line_start(line)..buffer.line_start(line + 1)
                } else if line > 0 && last_cut_end_line != Some(line) {
                    buffer.line_end(line - 1)..buffer.line_end(line)
                } else {
                    buffer.line_start(line)..buffer.line_end(line)
                };
                last_cut_end_line = Some(buffer.position_of(r.end).line);
                (!r.is_empty()).then_some(r)
            } else {
                Some(sel.range())
            };
            match range {
                Some(r) => {
                    if let Some(prev) = &last_emitted
                        && r.start < prev.end
                    {
                        // Overlap: drop this cursor's delete, land its caret
                        // where the winner's lands so the two merge.
                        anchors.push(prev.start);
                        continue;
                    }
                    anchors.push(r.start);
                    last_emitted = Some(r.clone());
                    edits.push(Edit::delete(r));
                }
                None => anchors.push(sel.head),
            }
        }

        if edits.is_empty() {
            return None;
        }

        // Post-change carets: the edits are pure deletions, sorted and
        // non-overlapping, so an anchor moves left by every deletion that ends
        // at or before it (no anchor sits strictly inside a deletion — each is
        // a deletion's own start, a merged winner's start, or an untouched
        // head on a line nothing deletes).
        let carets: Vec<Selection> = anchors
            .iter()
            .map(|&a| {
                let removed: usize = edits
                    .iter()
                    .take_while(|e| e.range.end <= a)
                    .map(|e| e.range.end - e.range.start)
                    .sum();
                Selection::cursor(a - removed)
            })
            .collect();

        let after = Selections::new(carets, before.primary_index());
        Some(self.commit(EditKind::Delete, false, edits, after))
    }

    /// Paste `text` at every cursor — Monaco's `PasteOperation.getEdits`.
    ///
    /// The two flags come from the copy metadata, and the caller passes them
    /// only when the clipboard still holds the copied text (otherwise both are
    /// off and this is a plain multi-cursor insert). With several cursors the
    /// distribution decision runs first: a `multicursor_text` whose piece
    /// count matches the cursor count hands one piece to each; failing that,
    /// text whose line count matches spreads line-per-cursor (Monaco's default
    /// `multiCursorPaste: 'spread'`, after stripping one trailing newline).
    ///
    /// Undistributed paste inserts the whole text at each cursor, except the
    /// whole-line special case: text from a single-empty-cursor copy
    /// (`paste_on_new_line`) that is exactly one line ending in `\n` inserts
    /// at the start of the caret's line and *preserves* the caret — the copied
    /// line lands above the current one. The flag latches off at the first
    /// non-empty selection, for that cursor and all later ones, as in Monaco.
    ///
    /// Line endings in the pasted text are normalized to the model's `\n`; the
    /// distribution decisions run on the raw text first, as Monaco's do.
    pub fn paste(
        &mut self,
        text: &str,
        paste_on_new_line: bool,
        multicursor_text: Option<&[String]>,
    ) -> Option<EditSummary> {
        if text.is_empty() {
            return None;
        }
        let before = self.selections().clone();
        let distributed = distribute(&before, text, paste_on_new_line, multicursor_text);

        // The whole-line insert applies only to text that is exactly one line
        // ending in `\n` (the byte `\n` both first and last), and never when
        // distributing.
        let mut pon =
            distributed.is_none() && paste_on_new_line && text.find('\n') == Some(text.len() - 1);

        let buffer = self.buffer();
        let mut edits: Vec<Edit> = Vec::with_capacity(before.len());
        // Caret plans: an explicit post-change offset, or — for a whole-line
        // insert — the preserved pre-change head, resolved after the walk
        // (a later cursor's insert on the same line shifts it too).
        enum Plan {
            At(usize),
            PreservedHead(usize),
        }
        let mut plans: Vec<Plan> = Vec::with_capacity(before.len());
        let mut pon_inserts: Vec<(usize, usize)> = Vec::new(); // (offset, chars)
        let mut delta: isize = 0;

        for (i, sel) in before.iter().enumerate() {
            let piece = match &distributed {
                Some(pieces) => pieces[i].as_str(),
                None => text,
            };
            let insert = normalize_eol(piece);
            let inserted = insert.chars().count();
            if pon && !sel.is_empty() {
                pon = false;
            }
            if pon {
                let at = buffer.line_start(buffer.position_of(sel.head).line);
                pon_inserts.push((at, inserted));
                plans.push(Plan::PreservedHead(sel.head));
                delta += inserted as isize;
                edits.push(Edit::insert(at, insert));
            } else {
                let range = sel.range();
                let removed = range.end - range.start;
                plans.push(Plan::At((range.start as isize + delta) as usize + inserted));
                delta += inserted as isize - removed as isize;
                // An empty piece over an empty cursor is nothing at all — keep
                // it out of the change so history stays honest.
                if removed > 0 || inserted > 0 {
                    edits.push(Edit::replace(range, insert));
                }
            }
        }

        if edits.is_empty() {
            return None;
        }

        let carets: Vec<Selection> = plans
            .into_iter()
            .map(|plan| {
                Selection::cursor(match plan {
                    Plan::At(offset) => offset,
                    // A preserved head shifts right by every whole-line insert
                    // at or before it — its own included, which is exactly what
                    // keeps the caret on its text as the line moves down
                    // (Monaco's tracked selection with forceMoveMarkers).
                    Plan::PreservedHead(head) => {
                        head + pon_inserts
                            .iter()
                            .filter(|(at, _)| *at <= head)
                            .map(|(_, n)| n)
                            .sum::<usize>()
                    }
                })
            })
            .collect();

        let after = Selections::new(carets, before.primary_index());
        Some(self.commit(EditKind::Insert, false, edits, after))
    }
}

/// Monaco's `_distributePasteToCursors`: `None` means paste the whole text at
/// every cursor; `Some` hands piece `i` to cursor `i` (selection order).
fn distribute(
    sels: &Selections,
    text: &str,
    paste_on_new_line: bool,
    multicursor_text: Option<&[String]>,
) -> Option<Vec<String>> {
    if sels.len() == 1 {
        return None;
    }
    // A matching multicursor copy wins outright — Monaco checks it before the
    // paste-on-new-line bailout.
    if let Some(pieces) = multicursor_text
        && pieces.len() == sels.len()
    {
        return Some(pieces.to_vec());
    }
    if paste_on_new_line {
        return None;
    }
    // 'spread': one trailing newline (\n, \r, or \r\n) is forgiven, then the
    // text spreads iff its line count matches the cursor count.
    let t = text.strip_suffix('\n').unwrap_or(text);
    let t = t.strip_suffix('\r').unwrap_or(t);
    let lines = split_lines(t);
    (lines.len() == sels.len()).then_some(lines)
}

/// Split on any line ending (`\r\n`, `\r`, or `\n`), keeping empty lines —
/// Monaco's `splitLines`.
fn split_lines(text: &str) -> Vec<String> {
    normalize_eol(text).split('\n').map(String::from).collect()
}

/// Rewrite any `\r\n` or lone `\r` to the model's `\n`. Monaco does the same
/// to every inserted string at the text-buffer level; here it happens as the
/// paste text becomes an edit.
fn normalize_eol(text: &str) -> String {
    if text.contains('\r') {
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text.to_string()
    }
}
