//! Grapheme-cluster boundaries over the document.
//!
//! Char arithmetic gets two things wrong that a user notices immediately: a
//! `\r\n` terminator is one keystroke to step over, not two, and a base
//! character with its combining marks (`e` + ´) is one character, not several.
//! Both cursor motion and deletion route through here so they agree.

use rinch_code_editor_buffer::Buffer;
use unicode_segmentation::UnicodeSegmentation;

/// The grapheme boundary immediately before `offset` — where a leftward step or
/// a backspace lands.
pub fn prev_boundary(buffer: &Buffer, offset: usize) -> usize {
    let offset = offset.min(buffer.len_chars());
    if offset == 0 {
        return 0;
    }
    let pos = buffer.position_of(offset);
    if pos.col == 0 {
        // At a line start, stepping left lands at the end of the previous line's
        // content, skipping the whole terminator (`\n` or `\r\n`) in one move.
        return buffer.line_end(pos.line - 1);
    }
    let line = buffer.line_text(pos.line);
    buffer.line_start(pos.line) + prev_within_line(&line, pos.col)
}

/// The grapheme boundary immediately after `offset` — where a rightward step or
/// a forward-delete lands.
pub fn next_boundary(buffer: &Buffer, offset: usize) -> usize {
    let len = buffer.len_chars();
    if offset >= len {
        return len;
    }
    let pos = buffer.position_of(offset);
    if pos.col >= buffer.line_len(pos.line) {
        // At the end of the line's content the next char is the terminator;
        // stepping right skips it to the start of the next line in one move.
        // `offset < len` guarantees a following line exists.
        return buffer.line_start(pos.line + 1);
    }
    let line = buffer.line_text(pos.line);
    buffer.line_start(pos.line) + next_within_line(&line, pos.col)
}

/// Char columns at which graphemes begin on a line: `0`, then the end of each
/// cluster. `line` must exclude the terminator (as [`Buffer::line_text`] gives).
fn boundaries(line: &str) -> Vec<usize> {
    let mut cols = vec![0];
    let mut col = 0;
    for g in line.graphemes(true) {
        col += g.chars().count();
        cols.push(col);
    }
    cols
}

fn prev_within_line(line: &str, col: usize) -> usize {
    boundaries(line)
        .into_iter()
        .rev()
        .find(|&b| b < col)
        .unwrap_or(0)
}

fn next_within_line(line: &str, col: usize) -> usize {
    boundaries(line)
        .into_iter()
        .find(|&b| b > col)
        .unwrap_or(col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_over_a_plain_char() {
        let b = Buffer::from_text("abc");
        assert_eq!(next_boundary(&b, 1), 2);
        assert_eq!(prev_boundary(&b, 1), 0);
    }

    #[test]
    fn a_combining_mark_moves_with_its_base() {
        // "e" + combining acute, then "x": chars are [e, ́, x].
        let b = Buffer::from_text("e\u{0301}x");
        assert_eq!(next_boundary(&b, 0), 2, "skip the e and its mark together");
        assert_eq!(prev_boundary(&b, 2), 0, "and back over both");
    }

    #[test]
    fn crlf_is_one_step() {
        // "ab\r\ncd": chars a0 b1 \r2 \n3 c4 d5.
        let b = Buffer::from_text("ab\r\ncd");
        // Stepping right from the end of line 0 content skips the whole \r\n.
        assert_eq!(next_boundary(&b, 2), 4);
        // And stepping left from the start of line 1 comes back over it.
        assert_eq!(prev_boundary(&b, 4), 2);
    }

    #[test]
    fn lf_line_boundaries() {
        let b = Buffer::from_text("ab\ncd");
        assert_eq!(next_boundary(&b, 2), 3, "over the \\n to line 1 start");
        assert_eq!(prev_boundary(&b, 3), 2, "back over the \\n");
    }

    #[test]
    fn clamps_at_the_ends() {
        let b = Buffer::from_text("ab");
        assert_eq!(prev_boundary(&b, 0), 0);
        assert_eq!(next_boundary(&b, 2), 2);
        assert_eq!(next_boundary(&b, 99), 2);
    }
}
