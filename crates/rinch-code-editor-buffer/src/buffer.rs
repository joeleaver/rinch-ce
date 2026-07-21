use ropey::Rope;
use std::ops::Range;

/// A line/column address. Both are zero-based; `col` counts **chars**, not bytes
/// and not display columns (a tab is one column here — expansion is the view's job).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Position {
    pub line: usize,
    pub col: usize,
}

impl Position {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// The text of one document, stored as a rope.
///
/// The rope is what makes a large file editable: insert and remove are O(log n)
/// rather than the O(n) memmove a flat `String` would pay on every keystroke.
#[derive(Debug, Clone, Default)]
pub struct Buffer {
    rope: Rope,
}

impl Buffer {
    pub fn new() -> Self {
        Self { rope: Rope::new() }
    }

    pub fn from_text(text: &str) -> Self {
        Self {
            rope: Rope::from_str(text),
        }
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// Total length in chars.
    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn is_empty(&self) -> bool {
        self.rope.len_chars() == 0
    }

    /// Number of lines. A trailing newline yields a final empty line, so
    /// `"a\n"` is 2 lines — the same convention every editor shows in its gutter.
    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    /// Char offset of the start of `line`. Clamped to the end of the buffer.
    pub fn line_start(&self, line: usize) -> usize {
        if line >= self.rope.len_lines() {
            return self.rope.len_chars();
        }
        self.rope.line_to_char(line)
    }

    /// Char offset of the end of `line`, **excluding** its line terminator.
    pub fn line_end(&self, line: usize) -> usize {
        self.line_start(line) + self.line_len(line)
    }

    /// Length of `line` in chars, excluding its `\n` or `\r\n` terminator.
    pub fn line_len(&self, line: usize) -> usize {
        if line >= self.rope.len_lines() {
            return 0;
        }
        let slice = self.rope.line(line);
        let mut len = slice.len_chars();
        if len > 0 && slice.char(len - 1) == '\n' {
            len -= 1;
            if len > 0 && slice.char(len - 1) == '\r' {
                len -= 1;
            }
        }
        len
    }

    /// Text of `line`, excluding its terminator.
    pub fn line_text(&self, line: usize) -> String {
        let start = self.line_start(line);
        self.slice(start..start + self.line_len(line))
    }

    /// Convert a char offset to a line/column address.
    pub fn position_of(&self, offset: usize) -> Position {
        let offset = offset.min(self.rope.len_chars());
        let line = self.rope.char_to_line(offset);
        Position::new(line, offset - self.rope.line_to_char(line))
    }

    /// Convert a line/column address to a char offset, clamping both components
    /// so an out-of-range caret (a stale position after a remote edit, say) can
    /// never panic — it lands at the nearest valid spot instead.
    pub fn offset_of(&self, pos: Position) -> usize {
        if pos.line >= self.rope.len_lines() {
            return self.rope.len_chars();
        }
        self.line_start(pos.line) + pos.col.min(self.line_len(pos.line))
    }

    pub fn slice(&self, range: Range<usize>) -> String {
        let end = range.end.min(self.rope.len_chars());
        let start = range.start.min(end);
        self.rope.slice(start..end).to_string()
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// Raw insert. Prefer [`crate::Change`] — this does not record undo.
    pub(crate) fn insert(&mut self, offset: usize, text: &str) {
        self.rope.insert(offset.min(self.rope.len_chars()), text);
    }

    /// Raw remove. Prefer [`crate::Change`] — this does not record undo.
    pub(crate) fn remove(&mut self, range: Range<usize>) {
        let end = range.end.min(self.rope.len_chars());
        let start = range.start.min(end);
        self.rope.remove(start..end);
    }
}

impl From<&str> for Buffer {
    fn from(s: &str) -> Self {
        Self::from_text(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_addressing_excludes_terminators() {
        let b = Buffer::from_text("alpha\nbeta\r\ngamma");
        assert_eq!(b.len_lines(), 3);
        assert_eq!(b.line_len(0), 5);
        assert_eq!(b.line_len(1), 4, "\\r\\n must not count toward the column");
        assert_eq!(b.line_len(2), 5);
        assert_eq!(b.line_text(1), "beta");
    }

    #[test]
    fn trailing_newline_yields_a_final_empty_line() {
        let b = Buffer::from_text("a\n");
        assert_eq!(b.len_lines(), 2);
        assert_eq!(b.line_text(1), "");
    }

    #[test]
    fn position_offset_roundtrip_across_multibyte_text() {
        // 'é' is 2 bytes but 1 char; '🦀' is 4 bytes but 1 char. Column arithmetic
        // must count chars, or the caret drifts on any non-ASCII line.
        let b = Buffer::from_text("héllo\n🦀🦀x");
        assert_eq!(b.line_len(1), 3);
        for offset in 0..=b.len_chars() {
            let pos = b.position_of(offset);
            assert_eq!(b.offset_of(pos), offset, "roundtrip failed at {offset}");
        }
    }

    #[test]
    fn out_of_range_positions_clamp_rather_than_panic() {
        let b = Buffer::from_text("ab\ncd");
        assert_eq!(b.offset_of(Position::new(99, 0)), b.len_chars());
        assert_eq!(b.offset_of(Position::new(0, 99)), 2);
        assert_eq!(b.position_of(9_999), Position::new(1, 2));
    }
}
