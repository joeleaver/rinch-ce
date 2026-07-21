//! Per-line token storage, mirroring Monaco's `LineTokens`.
//!
//! Tokens are held as a flat `Vec<u32>` of `[end, metadata, end, metadata, ...]`
//! pairs rather than a `Vec` of structs. One allocation per line instead of one
//! per token matters: the renderer walks these arrays for every visible line on
//! every frame, and a struct-of-two-fields would double the memory traffic and
//! scatter it across the heap.
//!
//! **Offsets are char offsets from the start of the line**, and each token's
//! stored offset is its *end*, exclusive. Monaco stores UTF-16 code-unit offsets
//! here; we store chars, to stay consistent with the buffer crate. The
//! conversion to bytes happens only where text layout demands it.

use crate::token::TokenMetadata;
use std::ops::Range;

/// One token's extent and presentation, as yielded during iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenSpan {
    /// Char offset from the start of the line, inclusive.
    pub start: u32,
    /// Char offset from the start of the line, exclusive.
    pub end: u32,
    pub metadata: TokenMetadata,
}

impl TokenSpan {
    pub fn range(&self) -> Range<u32> {
        self.start..self.end
    }

    pub fn len(&self) -> u32 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// The tokens of a single line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineTokens {
    /// Flat `[end0, meta0, end1, meta1, ...]`. Ends are strictly increasing.
    data: Vec<u32>,
}

impl LineTokens {
    /// A line with no tokens at all — distinct from a line with one default
    /// token, which is what an empty line in a tokenized document looks like.
    pub fn empty() -> Self {
        Self { data: Vec::new() }
    }

    /// A whole line as one default-styled token. Used for lines a highlighter
    /// declines to tokenize, and by the plain-text highlighter.
    pub fn single(len: u32, metadata: TokenMetadata) -> Self {
        Self {
            data: vec![len, metadata.raw()],
        }
    }

    pub fn count(&self) -> usize {
        self.data.len() / 2
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Char offset just past the last token — i.e. the tokenized length of the line.
    ///
    /// Note this is `data[len - 2]`, not `data.last()`: the array interleaves
    /// `[end, metadata]`, so the final element is a metadata word.
    pub fn line_len(&self) -> u32 {
        if self.data.is_empty() {
            0
        } else {
            self.data[self.data.len() - 2]
        }
    }

    /// Exclusive end offset of token `index`.
    pub fn end_of(&self, index: usize) -> u32 {
        self.data[index * 2]
    }

    /// Inclusive start offset of token `index` — the previous token's end.
    pub fn start_of(&self, index: usize) -> u32 {
        if index == 0 {
            0
        } else {
            self.data[(index - 1) * 2]
        }
    }

    pub fn metadata_of(&self, index: usize) -> TokenMetadata {
        TokenMetadata::from_raw(self.data[index * 2 + 1])
    }

    pub fn span(&self, index: usize) -> TokenSpan {
        TokenSpan {
            start: self.start_of(index),
            end: self.end_of(index),
            metadata: self.metadata_of(index),
        }
    }

    /// Index of the token containing `col`, by binary search.
    ///
    /// A token owns `[start, end)`, so an offset exactly at a boundary belongs to
    /// the token that *starts* there. `col` past the end of the line yields the
    /// last token, matching Monaco — the caret sitting past the final character
    /// still needs a style to render with.
    pub fn token_index_at(&self, col: u32) -> Option<usize> {
        let count = self.count();
        if count == 0 {
            return None;
        }
        if col >= self.line_len() {
            return Some(count - 1);
        }

        let (mut low, mut high) = (0usize, count - 1);
        while low < high {
            let mid = low + (high - low) / 2;
            if self.end_of(mid) <= col {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Some(low)
    }

    pub fn metadata_at(&self, col: u32) -> Option<TokenMetadata> {
        self.token_index_at(col).map(|i| self.metadata_of(i))
    }

    pub fn iter(&self) -> impl Iterator<Item = TokenSpan> + '_ {
        (0..self.count()).map(move |i| self.span(i))
    }

    /// Iterate only the tokens overlapping `cols`, clipped to it.
    ///
    /// The renderer draws a horizontally scrolled viewport; emitting tokens it
    /// will clip away anyway is wasted work on every frame of a long line.
    pub fn iter_range(&self, cols: Range<u32>) -> impl Iterator<Item = TokenSpan> + '_ {
        let first = if cols.start >= self.line_len() {
            self.count()
        } else {
            self.token_index_at(cols.start).unwrap_or(self.count())
        };
        (first..self.count())
            .map(move |i| self.span(i))
            .take_while(move |s| s.start < cols.end)
            .map(move |s| TokenSpan {
                start: s.start.max(cols.start),
                end: s.end.min(cols.end),
                metadata: s.metadata,
            })
            .filter(|s| !s.is_empty())
    }
}

/// Accumulates tokens for one line, enforcing the ordering invariant.
///
/// Highlighters produce tokens in document order but with gaps — tree-sitter
/// captures cover only what its queries match, leaving whitespace and
/// punctuation unclaimed. The builder fills those gaps with default-styled
/// tokens so the array always covers `[0, line_len)` contiguously, which is what
/// lets the renderer walk it without bounds checks.
#[derive(Debug, Default)]
pub struct LineTokensBuilder {
    data: Vec<u32>,
    last_end: u32,
}

impl LineTokensBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(tokens: usize) -> Self {
        Self {
            data: Vec::with_capacity(tokens * 2),
            last_end: 0,
        }
    }

    /// Push a token ending at `end` (exclusive, chars from line start).
    ///
    /// Ignores a token that does not advance past the previous one. Overlapping
    /// captures are normal in tree-sitter — an outer `function` capture and an
    /// inner `function.builtin` cover the same bytes — and the caller resolves
    /// precedence before calling; this is the backstop that keeps the invariant
    /// true regardless.
    pub fn push(&mut self, end: u32, metadata: TokenMetadata) {
        if end <= self.last_end {
            return;
        }
        // Merge with the previous token when the style is identical, so a run of
        // unstyled punctuation becomes one span instead of a dozen.
        if self.data.last() == Some(&metadata.raw()) {
            let last = self.data.len() - 2;
            self.data[last] = end;
            self.last_end = end;
            return;
        }
        self.data.push(end);
        self.data.push(metadata.raw());
        self.last_end = end;
    }

    /// Emit a default-styled token covering `[last_end, up_to)` if there is a gap.
    pub fn fill_gap_to(&mut self, up_to: u32, default: TokenMetadata) {
        if up_to > self.last_end {
            self.push(up_to, default);
        }
    }

    pub fn last_end(&self) -> u32 {
        self.last_end
    }

    /// Finish, padding to `line_len` so the array always covers the whole line.
    pub fn build(mut self, line_len: u32, default: TokenMetadata) -> LineTokens {
        self.fill_gap_to(line_len, default);
        LineTokens { data: self.data }
    }
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

    fn build(tokens: &[(u32, u16)], line_len: u32) -> LineTokens {
        let mut b = LineTokensBuilder::new();
        for &(end, fg) in tokens {
            b.push(end, meta(fg));
        }
        b.build(line_len, meta(0))
    }

    #[test]
    fn spans_are_contiguous_from_zero() {
        let t = build(&[(3, 1), (7, 2), (10, 3)], 10);
        let spans: Vec<_> = t.iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(spans, vec![(0, 3), (3, 7), (7, 10)]);
    }

    #[test]
    fn trailing_gap_is_filled_to_line_length() {
        let t = build(&[(3, 1)], 10);
        assert_eq!(t.count(), 2);
        assert_eq!(t.span(1).range(), 3..10);
        assert_eq!(t.line_len(), 10);
    }

    #[test]
    fn adjacent_tokens_with_identical_style_are_merged() {
        let t = build(&[(3, 1), (7, 1), (10, 1)], 10);
        assert_eq!(t.count(), 1, "one run, not three");
        assert_eq!(t.span(0).range(), 0..10);
    }

    #[test]
    fn non_advancing_tokens_are_ignored() {
        // An inner capture that ends where the outer one already ended.
        let t = build(&[(5, 1), (5, 2), (3, 3), (8, 4)], 8);
        let spans: Vec<_> = t.iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(spans, vec![(0, 5), (5, 8)]);
    }

    #[test]
    fn binary_search_finds_the_owning_token() {
        let t = build(&[(3, 1), (7, 2), (10, 3)], 10);
        assert_eq!(t.token_index_at(0), Some(0));
        assert_eq!(t.token_index_at(2), Some(0));
        assert_eq!(
            t.token_index_at(3),
            Some(1),
            "a boundary belongs to the token starting there"
        );
        assert_eq!(t.token_index_at(6), Some(1));
        assert_eq!(t.token_index_at(7), Some(2));
        assert_eq!(t.token_index_at(9), Some(2));
    }

    #[test]
    fn search_past_end_of_line_yields_the_last_token() {
        // The caret can sit past the final character and still needs a style.
        let t = build(&[(3, 1), (10, 2)], 10);
        assert_eq!(t.token_index_at(10), Some(1));
        assert_eq!(t.token_index_at(9_999), Some(1));
    }

    #[test]
    fn binary_search_agrees_with_linear_scan_at_every_offset() {
        let t = build(&[(1, 1), (2, 2), (5, 3), (9, 4), (13, 5), (14, 6)], 20);
        for col in 0..t.line_len() {
            let found = t.token_index_at(col).unwrap();
            let expected = (0..t.count())
                .find(|&i| t.start_of(i) <= col && col < t.end_of(i))
                .unwrap();
            assert_eq!(found, expected, "mismatch at col {col}");
        }
    }

    #[test]
    fn empty_line_has_no_tokens() {
        let t = build(&[], 0);
        assert_eq!(t.count(), 0);
        assert!(t.token_index_at(0).is_none());
        assert!(t.metadata_at(0).is_none());
    }

    #[test]
    fn iter_range_clips_to_the_viewport() {
        let t = build(&[(3, 1), (7, 2), (10, 3)], 10);
        let spans: Vec<_> = t.iter_range(2..8).map(|s| (s.start, s.end)).collect();
        assert_eq!(
            spans,
            vec![(2, 3), (3, 7), (7, 8)],
            "partial tokens are clipped, not dropped"
        );
    }

    #[test]
    fn iter_range_beyond_the_line_yields_nothing() {
        let t = build(&[(3, 1), (10, 2)], 10);
        assert_eq!(t.iter_range(20..30).count(), 0);
    }

    #[test]
    fn iter_range_covering_everything_matches_iter() {
        let t = build(&[(3, 1), (7, 2), (10, 3)], 10);
        let full: Vec<_> = t.iter().collect();
        let ranged: Vec<_> = t.iter_range(0..t.line_len()).collect();
        assert_eq!(full, ranged);
    }

    #[test]
    fn single_covers_the_whole_line() {
        let t = LineTokens::single(12, meta(4));
        assert_eq!(t.count(), 1);
        assert_eq!(t.span(0).range(), 0..12);
        assert_eq!(t.metadata_at(5), Some(meta(4)));
    }
}
