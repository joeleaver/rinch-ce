//! Cursor motion. Pure functions from a selection set to a new one — no buffer
//! mutation, no rinch. Every motion applies to every selection independently and
//! the result is re-normalized, so multi-cursor works for free.
//!
//! Two flavours per motion, chosen by `extend`: a plain move collapses each
//! selection onto its new caret; an extending move (shift held) keeps the anchor
//! and drags only the head.

use rinch_code_editor_buffer::{Buffer, Position, Selection, Selections};

use crate::geometry::Grid;
use crate::graphemes;

/// A cursor motion. `Up`/`Down` carry a line count so page motion is just a
/// large step: `Up(1)` is the up arrow, `Up(page_lines)` is Page Up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up(usize),
    Down(usize),
    /// Smart Home: to the first non-whitespace character, or to column 0 if the
    /// caret is already there.
    LineStart,
    /// To the end of the line's content (before its terminator).
    LineEnd,
    WordLeft,
    WordRight,
    DocStart,
    DocEnd,
}

/// Apply `motion` to every selection. `grid` is consulted only by vertical
/// motions (to preserve the on-screen column across lines of differing width);
/// the others ignore it.
pub fn move_cursors(
    buffer: &Buffer,
    grid: &Grid,
    selections: &Selections,
    motion: Motion,
    extend: bool,
) -> Selections {
    let primary = selections.primary_index();
    let moved: Vec<Selection> = selections
        .iter()
        .map(|sel| move_one(buffer, grid, *sel, motion, extend))
        .collect();
    Selections::new(moved, primary)
}

fn move_one(
    buffer: &Buffer,
    grid: &Grid,
    sel: Selection,
    motion: Motion,
    extend: bool,
) -> Selection {
    // Vertical motion is the only one that carries a goal column forward; every
    // other motion sets the caret's column afresh and so clears it.
    let (head, goal) = match motion {
        Motion::Up(n) => vertical(buffer, grid, sel, Dir::Up, n),
        Motion::Down(n) => vertical(buffer, grid, sel, Dir::Down, n),
        Motion::Left => (horizontal_left(buffer, sel, extend), None),
        Motion::Right => (horizontal_right(buffer, sel, extend), None),
        Motion::LineStart => (smart_home(buffer, sel.head), None),
        Motion::LineEnd => (line_end(buffer, sel.head), None),
        Motion::WordLeft => (word_left(buffer, sel.head), None),
        Motion::WordRight => (word_right(buffer, sel.head), None),
        Motion::DocStart => (0, None),
        Motion::DocEnd => (buffer.len_chars(), None),
    };
    let anchor = if extend { sel.anchor } else { head };
    Selection {
        anchor,
        head,
        goal_col: goal,
    }
}

// ---- horizontal ----------------------------------------------------------

fn horizontal_left(buffer: &Buffer, sel: Selection, extend: bool) -> usize {
    // A plain left arrow on a selection collapses to its left edge rather than
    // moving a character — the behaviour every editor has.
    if !extend && !sel.is_empty() {
        sel.start()
    } else {
        graphemes::prev_boundary(buffer, sel.head)
    }
}

fn horizontal_right(buffer: &Buffer, sel: Selection, extend: bool) -> usize {
    if !extend && !sel.is_empty() {
        sel.end()
    } else {
        graphemes::next_boundary(buffer, sel.head)
    }
}

// ---- vertical (the only carrier of goal_col) -----------------------------

#[derive(Clone, Copy)]
enum Dir {
    Up,
    Down,
}

fn vertical(
    buffer: &Buffer,
    grid: &Grid,
    sel: Selection,
    dir: Dir,
    n: usize,
) -> (usize, Option<usize>) {
    let pos = buffer.position_of(sel.head);
    let cur_line_text = buffer.line_text(pos.line);
    // The column we *want* to be in: the one remembered from an earlier vertical
    // move, or — starting a fresh vertical run — the caret's current visual
    // column. Remembering the visual column is what lets the caret pass through a
    // short line and come back out at the same x.
    let goal = sel
        .goal_col
        .unwrap_or_else(|| grid.visual_column(&cur_line_text, pos.col));

    let last_line = buffer.len_lines().saturating_sub(1);

    // At the top edge, Up parks at the very start of the document; at the bottom
    // edge, Down parks at the very end — matching Monaco, and giving a way to
    // reach the line ends by keyboard.
    let target_line = match dir {
        Dir::Up if pos.line == 0 => return (0, Some(0)),
        Dir::Up => pos.line.saturating_sub(n),
        Dir::Down if pos.line == last_line => {
            let end = buffer.len_chars();
            let end_pos = buffer.position_of(end);
            let end_text = buffer.line_text(end_pos.line);
            return (end, Some(grid.visual_column(&end_text, end_pos.col)));
        }
        Dir::Down => (pos.line + n).min(last_line),
    };

    let target_text = buffer.line_text(target_line);
    let col = grid.char_column_at_visual(&target_text, goal as f32);
    let offset = buffer.offset_of(Position::new(target_line, col));
    (offset, Some(goal))
}

// ---- line boundaries -----------------------------------------------------

fn smart_home(buffer: &Buffer, head: usize) -> usize {
    let pos = buffer.position_of(head);
    let line = buffer.line_text(pos.line);
    let indent = line.chars().take_while(|c| *c == ' ' || *c == '\t').count();
    // Toggle: from anywhere but the indent, go to the indent; from the indent,
    // go to column 0.
    let target_col = if pos.col == indent { 0 } else { indent };
    buffer.line_start(pos.line) + target_col
}

fn line_end(buffer: &Buffer, head: usize) -> usize {
    buffer.line_end(buffer.position_of(head).line)
}

// ---- word motion ---------------------------------------------------------

#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Word,
    Space,
    Other,
}

/// Monaco-style classification: identifier characters (alphanumeric and `_`) are
/// one class so `snake_case` is a single word, whitespace another, and every
/// other symbol a third so a run of punctuation is skipped as a unit.
fn class(c: char) -> Class {
    if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// To the far side of the next word: skip whitespace, then skip the following run
/// of a single class. Newlines are whitespace, so this crosses line ends.
fn word_right(buffer: &Buffer, offset: usize) -> usize {
    let rope = buffer.rope();
    let len = buffer.len_chars();
    let mut i = offset;
    while i < len && class(rope.char(i)) == Class::Space {
        i += 1;
    }
    if i < len {
        let c = class(rope.char(i));
        while i < len && class(rope.char(i)) == c {
            i += 1;
        }
    }
    i
}

/// The mirror of [`word_right`]: skip whitespace to the left, then the run of a
/// single class, landing at that word's start.
fn word_left(buffer: &Buffer, offset: usize) -> usize {
    let rope = buffer.rope();
    let mut i = offset;
    while i > 0 && class(rope.char(i - 1)) == Class::Space {
        i -= 1;
    }
    if i > 0 {
        let c = class(rope.char(i - 1));
        while i > 0 && class(rope.char(i - 1)) == c {
            i -= 1;
        }
    }
    i
}
