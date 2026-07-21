//! Flattening overlapping tree-sitter captures into per-line token runs.
//!
//! # Precedence
//!
//! Highlight captures nest and overlap freely: a call expression may be captured
//! as `function`, and the identifier inside it as `function.builtin`, covering
//! part of the same bytes. tree-sitter-highlight resolves this two ways, and we
//! match both:
//!
//! - **Innermost wins.** The more deeply nested capture is the more specific
//!   description of those bytes.
//! - **On an exact tie, the earliest pattern in the query wins.** This is why
//!   `highlights.scm` files put their most specific patterns first.
//!
//! Both fall out of one sort plus painting in order: sort by start ascending,
//! end *descending*, pattern index *descending*, then paint. Whatever paints
//! last wins, and that ordering makes the last painter the innermost, earliest
//! pattern.

use crate::line_tokens::{LineTokens, LineTokensBuilder};
use crate::token::TokenMetadata;
use rinch_code_editor_buffer::Buffer;
use std::ops::Range;

/// One highlight capture, in document byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Capture {
    pub start_byte: usize,
    pub end_byte: usize,
    pub pattern_index: usize,
    pub metadata: TokenMetadata,
}

/// Order captures so that painting front-to-back yields the precedence rules
/// described in the module docs.
pub(crate) fn sort_captures(captures: &mut [Capture]) {
    captures.sort_by(|a, b| {
        a.start_byte
            .cmp(&b.start_byte)
            .then(b.end_byte.cmp(&a.end_byte))
            .then(b.pattern_index.cmp(&a.pattern_index))
    });
}

/// Render `lines` into `out`, one [`LineTokens`] per line.
///
/// `captures` must already be sorted by [`sort_captures`]. `scratch` is caller-
/// owned so the per-line metadata buffer is reused across lines and frames
/// rather than reallocated for each.
pub(crate) fn paint_lines(
    buffer: &Buffer,
    lines: Range<usize>,
    captures: &[Capture],
    default: TokenMetadata,
    scratch: &mut Vec<TokenMetadata>,
    out: &mut Vec<LineTokens>,
) {
    let rope = buffer.rope();
    let end_line = lines.end.min(buffer.len_lines());

    for line in lines.start..end_line {
        let line_len = buffer.line_len(line);
        if line_len == 0 {
            out.push(LineTokens::empty());
            continue;
        }

        let line_start_char = buffer.line_start(line);
        let line_start_byte = rope.char_to_byte(line_start_char);
        let line_end_byte = rope.char_to_byte(line_start_char + line_len);

        scratch.clear();
        scratch.resize(line_len, default);

        for capture in captures {
            // Sorted by start, so nothing further along can overlap this line.
            if capture.start_byte >= line_end_byte {
                break;
            }
            if capture.end_byte <= line_start_byte {
                continue;
            }

            let from_byte = capture.start_byte.max(line_start_byte);
            let to_byte = capture.end_byte.min(line_end_byte);
            if from_byte >= to_byte {
                continue;
            }

            // Byte offsets from tree-sitter can land mid-codepoint only if the
            // grammar is broken; byte_to_char would panic there, so clamp to a
            // char boundary rather than trusting it.
            let from_col =
                rope.byte_to_char(floor_char_boundary(rope, from_byte)) - line_start_char;
            let to_col = rope.byte_to_char(floor_char_boundary(rope, to_byte)) - line_start_char;

            for slot in &mut scratch[from_col.min(line_len)..to_col.min(line_len)] {
                *slot = capture.metadata;
            }
        }

        out.push(run_length_encode(scratch, default));
    }
}

/// Largest char-boundary byte offset at or below `byte`.
fn floor_char_boundary(rope: &ropey::Rope, byte: usize) -> usize {
    let byte = byte.min(rope.len_bytes());
    if rope.try_byte_to_char(byte).is_ok() {
        byte
    } else {
        // At most 3 bytes back to the start of a UTF-8 sequence.
        (1..=3)
            .map(|back| byte.saturating_sub(back))
            .find(|&b| rope.try_byte_to_char(b).is_ok())
            .unwrap_or(0)
    }
}

/// Collapse a per-char metadata array into token runs.
fn run_length_encode(per_char: &[TokenMetadata], default: TokenMetadata) -> LineTokens {
    let mut builder = LineTokensBuilder::new();
    let mut run_start = 0usize;
    for i in 1..=per_char.len() {
        let ended = i == per_char.len() || per_char[i] != per_char[run_start];
        if ended {
            builder.push(i as u32, per_char[run_start]);
            run_start = i;
        }
    }
    builder.build(per_char.len() as u32, default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::{ColorId, FontStyle, StandardTokenType};

    fn meta(fg: u16) -> TokenMetadata {
        TokenMetadata::new(
            0,
            StandardTokenType::Other,
            FontStyle::NONE,
            ColorId(fg),
            ColorId::UNSET,
        )
    }

    fn capture(range: Range<usize>, pattern: usize, fg: u16) -> Capture {
        Capture {
            start_byte: range.start,
            end_byte: range.end,
            pattern_index: pattern,
            metadata: meta(fg),
        }
    }

    fn paint(text: &str, mut captures: Vec<Capture>) -> Vec<LineTokens> {
        let buffer = Buffer::from_text(text);
        sort_captures(&mut captures);
        let mut scratch = Vec::new();
        let mut out = Vec::new();
        paint_lines(
            &buffer,
            0..buffer.len_lines(),
            &captures,
            meta(0),
            &mut scratch,
            &mut out,
        );
        out
    }

    fn runs(line: &LineTokens) -> Vec<(u32, u32, u16)> {
        line.iter()
            .map(|s| (s.start, s.end, s.metadata.foreground().0))
            .collect()
    }

    #[test]
    fn a_single_capture_splits_the_line_into_three_runs() {
        let out = paint("let x = 1", vec![capture(0..3, 0, 5)]);
        assert_eq!(runs(&out[0]), vec![(0, 3, 5), (3, 9, 0)]);
    }

    #[test]
    fn uncaptured_text_gets_the_default_style() {
        let out = paint("abc", vec![]);
        assert_eq!(runs(&out[0]), vec![(0, 3, 0)]);
    }

    #[test]
    fn a_nested_capture_overrides_its_parent() {
        // Outer 0..9 as color 1, inner 4..5 as color 2. Innermost wins.
        let out = paint("let x = 1", vec![capture(0..9, 0, 1), capture(4..5, 1, 2)]);
        assert_eq!(runs(&out[0]), vec![(0, 4, 1), (4, 5, 2), (5, 9, 1)]);
    }

    #[test]
    fn on_an_exact_range_tie_the_earlier_pattern_wins() {
        // Same bytes captured by pattern 0 and pattern 3; pattern 0 must win,
        // matching tree-sitter-highlight's "first pattern in the query" rule.
        let out = paint("abcd", vec![capture(0..4, 3, 9), capture(0..4, 0, 7)]);
        assert_eq!(runs(&out[0]), vec![(0, 4, 7)]);
    }

    #[test]
    fn partially_overlapping_captures_layer_by_start_order() {
        let out = paint("abcdef", vec![capture(0..4, 0, 1), capture(2..6, 1, 2)]);
        assert_eq!(runs(&out[0]), vec![(0, 2, 1), (2, 6, 2)]);
    }

    #[test]
    fn a_capture_spanning_lines_is_clipped_to_each() {
        // A block comment covering all of line 0 and part of line 1.
        let out = paint("abc\ndefgh", vec![capture(0..6, 0, 3)]);
        assert_eq!(runs(&out[0]), vec![(0, 3, 3)]);
        assert_eq!(runs(&out[1]), vec![(0, 2, 3), (2, 5, 0)]);
    }

    #[test]
    fn line_terminators_are_never_part_of_a_token() {
        // The capture covers the '\n' at byte 3, but tokens stop at line length.
        let out = paint("abc\ndef", vec![capture(0..5, 0, 4)]);
        assert_eq!(out[0].line_len(), 3, "the newline is not a column");
        assert_eq!(runs(&out[1]), vec![(0, 1, 4), (1, 3, 0)]);
    }

    #[test]
    fn empty_lines_produce_no_tokens() {
        let out = paint("a\n\nb", vec![capture(0..4, 0, 2)]);
        assert_eq!(out[1].count(), 0);
    }

    #[test]
    fn multibyte_text_maps_bytes_to_the_right_columns() {
        // "é" occupies bytes 0..2 but column 0..1; "🦀" bytes 2..6, columns 1..2.
        let out = paint("é🦀x", vec![capture(2..6, 0, 6)]);
        assert_eq!(
            runs(&out[0]),
            vec![(0, 1, 0), (1, 2, 6), (2, 3, 0)],
            "columns are chars even though captures are bytes"
        );
    }

    #[test]
    fn a_capture_landing_mid_codepoint_does_not_panic() {
        // Byte 1 is inside 'é'. A broken grammar could emit this; clamp, don't crash.
        let out = paint("éx", vec![capture(1..3, 0, 8)]);
        assert_eq!(out[0].line_len(), 2);
    }

    #[test]
    fn adjacent_identical_captures_merge_into_one_run() {
        let out = paint("abcdef", vec![capture(0..3, 0, 5), capture(3..6, 1, 5)]);
        assert_eq!(runs(&out[0]), vec![(0, 6, 5)], "same style, one span");
    }

    #[test]
    fn captures_entirely_outside_the_painted_lines_are_ignored() {
        let buffer = Buffer::from_text("aaa\nbbb\nccc");
        let mut captures = vec![capture(0..3, 0, 1), capture(8..11, 1, 2)];
        sort_captures(&mut captures);
        let mut scratch = Vec::new();
        let mut out = Vec::new();
        // Paint only the middle line.
        paint_lines(&buffer, 1..2, &captures, meta(0), &mut scratch, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(runs(&out[0]), vec![(0, 3, 0)], "neither capture reaches it");
    }
}
