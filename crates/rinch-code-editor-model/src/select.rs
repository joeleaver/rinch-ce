//! Mouse selection gestures: what a double or triple click selects, and how
//! the selection grows when the mouse drags afterwards.
//!
//! A direct port of Monaco's word and line gestures — `WordOperations.word`
//! (cursorWordOperations.ts) and `CursorMoveCommands.line`
//! (cursorMoveCommands.ts), both halves of each: the `!inSelectionMode` entry
//! (the click) and the `inSelectionMode` continuation (the drag). The
//! original clicked range plays Monaco's `selectionStart`: it stays inside
//! the selection whatever direction the drag goes, because the anchor flips
//! to whichever of its ends is behind the head (`_computeSelection`).
//!
//! Words here are the cursor-motion classifier's ([`crate::cursor`]): maximal
//! runs of word material (everything that is not whitespace or a separator —
//! identifiers, CJK, emoji), of separator punctuation (a run like `===`), or
//! of whitespace — the same three classes Monaco's `WordCharacterClassifier`
//! yields with its default `wordSeparators`.

use std::ops::Range;

use rinch_code_editor_buffer::{Buffer, Selection};

use crate::cursor::{Class, class};

/// A maximal single-class run on one line, in line-local char columns.
struct Run {
    class: Class,
    start: usize,
    end: usize,
}

/// The non-space runs of `line`, in order.
fn word_runs(line: &str) -> Vec<Run> {
    let mut runs = Vec::new();
    let mut cur: Option<Run> = None;
    let flush = |run: Option<Run>, runs: &mut Vec<Run>| {
        if let Some(run) = run
            && run.class != Class::Space
        {
            runs.push(run);
        }
    };
    for (col, ch) in line.chars().enumerate() {
        let c = class(ch);
        match &mut cur {
            Some(run) if run.class == c => run.end += 1,
            _ => {
                flush(cur.take(), &mut runs);
                cur = Some(Run {
                    class: c,
                    start: col,
                    end: col + 1,
                });
            }
        }
    }
    flush(cur.take(), &mut runs);
    runs
}

/// Monaco's `_findPreviousWordOnLine`: the nearest non-space run at or left of
/// the character before column `c` — i.e. the last run starting before `c`.
fn prev_word(runs: &[Run], c: usize) -> Option<&Run> {
    runs.iter().rev().find(|r| r.start < c)
}

/// Monaco's `_findNextWordOnLine`: the nearest non-space run at or right of
/// the character at column `c` — i.e. the first run ending after `c`.
fn next_word(runs: &[Run], c: usize) -> Option<&Run> {
    runs.iter().find(|r| r.end > c)
}

/// What a double click at `offset` selects — `WordOperations.word`, the entry
/// half. A click touching a word (either boundary included) takes the word; a
/// click on a separator run takes the run, but its *end* boundary defers to
/// whatever follows; a click in the open takes the whitespace gap between the
/// neighbouring words (or the line edge where there is none).
pub fn word_range_at(buffer: &Buffer, offset: usize) -> Range<usize> {
    let pos = buffer.position_of(offset);
    let text = buffer.line_text(pos.line);
    let runs = word_runs(&text);
    let c = pos.col;

    let prev = prev_word(&runs, c);
    let next = next_word(&runs, c);

    let (start, end) = if let Some(p) = prev.filter(|p| p.class == Class::Word && c <= p.end) {
        (p.start, p.end)
    } else if let Some(p) = prev.filter(|p| p.class == Class::Other && c < p.end) {
        (p.start, p.end)
    } else if let Some(n) = next.filter(|n| n.class == Class::Word && n.start <= c) {
        (n.start, n.end)
    } else if let Some(n) = next.filter(|n| n.class == Class::Other && n.start <= c) {
        (n.start, n.end)
    } else {
        (
            prev.map(|p| p.end).unwrap_or(0),
            next.map(|n| n.start)
                .unwrap_or_else(|| text.chars().count()),
        )
    };

    let ls = buffer.line_start(pos.line);
    ls + start..ls + end
}

/// What a triple click at `offset` selects — the whole line through its
/// terminator (`CursorMoveCommands.line`): line start to the next line's
/// start, which on the last line clamps to the document end, exactly Monaco's
/// last-line max-column case.
pub fn line_range_at(buffer: &Buffer, offset: usize) -> Range<usize> {
    let line = buffer.position_of(offset).line;
    buffer.line_start(line)..buffer.line_start(line + 1)
}

/// Continue a word-mode drag: the selection from the original double-clicked
/// range (`initial`) to the pointer at `offset` — `WordOperations.word`, the
/// drag half. The head snaps outward to the boundary of a word the pointer is
/// strictly inside (separator runs and boundaries do not snap); a pointer
/// back inside the original range restores exactly that range.
pub fn word_drag(buffer: &Buffer, initial: &Range<usize>, offset: usize) -> Selection {
    let pos = buffer.position_of(offset);
    let text = buffer.line_text(pos.line);
    let runs = word_runs(&text);
    let c = pos.col;

    // The snap candidate, Monaco's startColumn/endColumn pair: the word's
    // bounds when strictly inside one, else the exact pointer position.
    let ls = buffer.line_start(pos.line);
    let (snap_start, snap_end) =
        match prev_word(&runs, c).filter(|p| p.class == Class::Word && p.start < c && c < p.end) {
            Some(p) => (ls + p.start, ls + p.end),
            None => (offset, offset),
        };

    let contains = |o: usize| initial.start <= o && o <= initial.end;
    let head = if contains(offset) {
        initial.end
    } else if offset < initial.start {
        // Dragging backwards: the head is the snapped start — unless the snap
        // itself lands back inside the original range.
        if contains(snap_start) {
            initial.end
        } else {
            snap_start
        }
    } else {
        if contains(snap_end) {
            initial.start
        } else {
            snap_end
        }
    };

    extend_from(initial, head)
}

/// Continue a line-mode drag — `CursorMoveCommands.line`, the continuation:
/// below the original line the head is the start of the line *after* the
/// pointer's (whole lines stay selected), above it the start of the pointer's
/// line, and back on it the original range's end.
pub fn line_drag(buffer: &Buffer, initial: &Range<usize>, offset: usize) -> Selection {
    let entered = buffer.position_of(initial.start).line;
    let line = buffer.position_of(offset).line;
    let head = if line < entered {
        buffer.line_start(line)
    } else if line > entered {
        buffer.line_start(line + 1)
    } else {
        initial.end
    };
    extend_from(initial, head)
}

/// Extend a gesture's selection to `head` — Monaco's
/// `SingleCursorState._computeSelection`: the anchor is whichever end of the
/// original range keeps it inside the selection — its end when the head is at
/// or behind its start, its start otherwise. An empty original range (a
/// shift-gesture from a plain caret) always anchors at itself.
///
/// Public because a Shift+click extends from the *stored* gesture range
/// directly (no word or line snapping), and the view layer keeps that range —
/// Monaco's `selectionStart` — across presses.
pub fn extend_from(initial: &Range<usize>, head: usize) -> Selection {
    if !initial.is_empty() && head <= initial.start {
        Selection::new(initial.end, head)
    } else {
        Selection::new(initial.start, head)
    }
}
