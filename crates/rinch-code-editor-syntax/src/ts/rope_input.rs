//! Feeding a rope to tree-sitter without flattening it to a `String`.
//!
//! Both tree-sitter entry points that read text — the parser and the query
//! cursor's predicate evaluation — accept chunked callbacks. Using them means a
//! 100 MB file is parsed straight out of the rope's leaves, with no intermediate
//! copy on any keystroke.
//!
//! # Byte offsets live here
//!
//! tree-sitter speaks UTF-8 **bytes** throughout, including `Point::column`,
//! while the rest of this workspace speaks chars. This module is the boundary
//! where that conversion happens, and it should be the only place it does.

use ropey::Rope;
use tree_sitter::{Node, Parser, Point, TextProvider, Tree};

use rinch_code_editor_buffer::Buffer;

/// Parse `buffer`, reusing `old_tree` for incremental reparse when supplied.
pub(crate) fn parse(parser: &mut Parser, buffer: &Buffer, old_tree: Option<&Tree>) -> Option<Tree> {
    let rope = buffer.rope();
    let len_bytes = rope.len_bytes();
    let mut callback = |byte: usize, _: Point| -> &[u8] {
        if byte >= len_bytes {
            return &[];
        }
        let (chunk, chunk_start_byte, _, _) = rope.chunk_at_byte(byte);
        &chunk.as_bytes()[byte - chunk_start_byte..]
    };
    parser.parse_with_options(&mut callback, old_tree, None)
}

fn as_bytes(s: &str) -> &[u8] {
    s.as_bytes()
}

/// Supplies node text to query predicates (`#eq?`, `#match?`) from the rope.
pub(crate) struct RopeProvider<'a>(pub &'a Rope);

impl<'a> TextProvider<&'a [u8]> for RopeProvider<'a> {
    type I = std::iter::Map<ropey::iter::Chunks<'a>, fn(&'a str) -> &'a [u8]>;

    fn text(&mut self, node: Node) -> Self::I {
        let start = self
            .0
            .byte_to_char(node.start_byte().min(self.0.len_bytes()));
        let end = self.0.byte_to_char(node.end_byte().min(self.0.len_bytes()));
        self.0
            .slice(start..end)
            .chunks()
            .map(as_bytes as fn(&'a str) -> &'a [u8])
    }
}

/// The tree-sitter [`Point`] (row, byte-column) for a char offset.
pub(crate) fn point_at_char(buffer: &Buffer, char_offset: usize) -> Point {
    let rope = buffer.rope();
    let char_offset = char_offset.min(rope.len_chars());
    let position = buffer.position_of(char_offset);
    let line_start_byte = rope.char_to_byte(buffer.line_start(position.line));
    Point {
        row: position.line,
        column: rope.char_to_byte(char_offset) - line_start_byte,
    }
}

/// Byte offset of a char offset.
pub(crate) fn byte_at_char(buffer: &Buffer, char_offset: usize) -> usize {
    let rope = buffer.rope();
    rope.char_to_byte(char_offset.min(rope.len_chars()))
}

/// Advance a [`Point`] as though `text` were typed at it.
///
/// Used to derive the `old_end_position`/`new_end_position` of an edit without
/// materializing the intermediate document those positions refer to.
pub(crate) fn advance_point(start: Point, text: &str) -> Point {
    match text.rfind('\n') {
        // The text spans lines: the end column is whatever follows the last
        // newline, measured from column zero of the new row.
        Some(last_newline) => Point {
            row: start.row + text.bytes().filter(|&b| b == b'\n').count(),
            column: text.len() - last_newline - 1,
        },
        None => Point {
            row: start.row,
            column: start.column + text.len(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_columns_are_bytes_not_chars() {
        // 'é' is two bytes. tree-sitter wants the byte column, so a caret after
        // it on line 1 is at column 2, not 1.
        let buffer = Buffer::from_text("ascii\néx");
        assert_eq!(point_at_char(&buffer, 0), Point { row: 0, column: 0 });
        assert_eq!(point_at_char(&buffer, 6), Point { row: 1, column: 0 });
        assert_eq!(point_at_char(&buffer, 7), Point { row: 1, column: 2 });
        assert_eq!(point_at_char(&buffer, 8), Point { row: 1, column: 3 });
    }

    #[test]
    fn point_past_the_end_clamps() {
        let buffer = Buffer::from_text("ab");
        assert_eq!(point_at_char(&buffer, 9_999), Point { row: 0, column: 2 });
    }

    #[test]
    fn advance_within_a_line_adds_bytes_to_the_column() {
        let start = Point { row: 3, column: 5 };
        assert_eq!(advance_point(start, "abc"), Point { row: 3, column: 8 });
        // Two bytes, one char.
        assert_eq!(advance_point(start, "é"), Point { row: 3, column: 7 });
    }

    #[test]
    fn advance_across_lines_resets_the_column() {
        let start = Point { row: 3, column: 5 };
        assert_eq!(advance_point(start, "a\nbc"), Point { row: 4, column: 2 });
        assert_eq!(advance_point(start, "a\nb\n"), Point { row: 5, column: 0 });
        assert_eq!(
            advance_point(start, "\n\n\nxy"),
            Point { row: 6, column: 2 }
        );
    }

    #[test]
    fn advance_by_nothing_is_the_identity() {
        let start = Point { row: 2, column: 7 };
        assert_eq!(advance_point(start, ""), start);
    }

    #[test]
    fn advance_agrees_with_recomputing_the_point_from_scratch() {
        // The property that matters: advancing a point by inserted text must
        // match what point_at_char would say about the resulting document.
        for insert in ["x", "hello", "a\nb", "\n", "line\nbreak\nhere", "é\nü"] {
            let base = "first line\nsecond line";
            let at_char = 14; // partway into line 1
            let mut text = String::from(base);
            let byte = Buffer::from_text(base).rope().char_to_byte(at_char);
            text.insert_str(byte, insert);

            let start = point_at_char(&Buffer::from_text(base), at_char);
            let advanced = advance_point(start, insert);

            let after = Buffer::from_text(&text);
            let expected = point_at_char(&after, at_char + insert.chars().count());
            assert_eq!(advanced, expected, "inserting {insert:?}");
        }
    }
}
