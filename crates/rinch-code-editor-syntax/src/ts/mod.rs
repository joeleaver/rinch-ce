//! The tree-sitter highlighting backend.

mod alloc;
pub mod languages;
mod paint;
mod rope_input;

use crate::highlighter::Highlighter;
use crate::line_tokens::LineTokens;
use crate::theme::Theme;
use crate::token::TokenMetadata;
use languages::LanguageConfig;
use paint::Capture;
use rinch_code_editor_buffer::{Buffer, Change};
use std::ops::Range;
use streaming_iterator::StreamingIterator;
use tree_sitter::{InputEdit, Language, Parser, Query, QueryCursor, Tree};

/// Failure to construct a highlighter. Both variants mean a programming or
/// packaging error rather than anything a user did, so they surface at
/// construction rather than being swallowed at render time.
#[derive(Debug)]
pub enum SyntaxError {
    /// The grammar is incompatible with this tree-sitter runtime version.
    Language(tree_sitter::LanguageError),
    /// The highlight query failed to compile against the grammar.
    Query(tree_sitter::QueryError),
}

impl std::fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Language(e) => write!(f, "incompatible grammar: {e}"),
            Self::Query(e) => write!(f, "invalid highlight query: {e}"),
        }
    }
}

impl std::error::Error for SyntaxError {}

/// Incremental tree-sitter highlighting for one buffer.
///
/// Holds a parse tree that is edited in place on every change, so a keystroke
/// reparses only the affected subtree rather than the file. Tokens are produced
/// per viewport, and the query is restricted to the requested byte range, so
/// cost tracks what is on screen rather than how large the document is.
pub struct TreeSitterHighlighter {
    parser: Parser,
    query: Query,
    tree: Option<Tree>,
    /// `capture_metadata[i]` is the resolved style for the query's capture `i`.
    /// Resolving scope names once at construction keeps the hot path to an
    /// array index instead of a string walk per token.
    capture_metadata: Vec<TokenMetadata>,
    theme: Theme,
    cursor: QueryCursor,
    captures: Vec<Capture>,
    scratch: Vec<TokenMetadata>,
}

impl TreeSitterHighlighter {
    /// Build a highlighter for `language`, styling captures from
    /// `highlight_query` (the contents of a grammar's `highlights.scm`).
    pub fn new(
        language: &Language,
        highlight_query: &str,
        theme: Theme,
    ) -> Result<Self, SyntaxError> {
        // Must precede every other tree-sitter call: on wasm the default
        // allocator corrupts memory, and swapping it after an allocation would
        // free foreign pointers. See `alloc`.
        alloc::install();

        let mut parser = Parser::new();
        parser
            .set_language(language)
            .map_err(SyntaxError::Language)?;

        let query = Query::new(language, highlight_query).map_err(SyntaxError::Query)?;
        let capture_metadata = query
            .capture_names()
            .iter()
            .map(|name| theme.resolve(name))
            .collect();

        Ok(Self {
            parser,
            query,
            tree: None,
            capture_metadata,
            theme,
            cursor: QueryCursor::new(),
            captures: Vec::new(),
            scratch: Vec::new(),
        })
    }

    /// Build a highlighter for one of the bundled languages.
    pub fn for_language(config: &LanguageConfig, theme: Theme) -> Result<Self, SyntaxError> {
        Self::new(&config.grammar(), config.highlights, theme)
    }

    /// Replace the theme, re-resolving capture styles. Cheap: the parse tree is
    /// untouched, because highlighting is derived state and colors are not in it.
    pub fn set_theme(&mut self, theme: Theme) {
        self.capture_metadata = self
            .query
            .capture_names()
            .iter()
            .map(|name| theme.resolve(name))
            .collect();
        self.theme = theme;
    }

    /// The current parse tree, if the buffer has been parsed.
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// Translate one applied [`Change`] into tree-sitter edits.
    ///
    /// Edits are applied in ascending order. The `i`th inverse edit's range
    /// start is already expressed in coordinates that include edits `0..i`, and
    /// that is exactly the shift the tree has accumulated by the time we reach
    /// it — so no separate delta tracking is needed here.
    ///
    /// The removed text comes from the change's inverse, which is why this needs
    /// only the post-change buffer and never the previous one.
    fn edit_tree(&mut self, buffer: &Buffer, change: &Change) {
        let Some(tree) = self.tree.as_mut() else {
            return;
        };
        for (edit, inverse) in change.edits().iter().zip(change.inverse()) {
            let start_char = inverse.range.start;
            let start_byte = rope_input::byte_at_char(buffer, start_char);
            let start_position = rope_input::point_at_char(buffer, start_char);

            let removed = inverse.text.as_str();
            let inserted = edit.text.as_str();

            tree.edit(&InputEdit {
                start_byte,
                old_end_byte: start_byte + removed.len(),
                new_end_byte: start_byte + inserted.len(),
                start_position,
                old_end_position: rope_input::advance_point(start_position, removed),
                new_end_position: rope_input::advance_point(start_position, inserted),
            });
        }
    }

    /// Gather every capture intersecting `bytes` into `self.captures`.
    fn collect_captures(&mut self, buffer: &Buffer, bytes: Range<usize>) {
        self.captures.clear();
        let Some(tree) = self.tree.as_ref() else {
            return;
        };

        self.cursor.set_byte_range(bytes);
        let provider = rope_input::RopeProvider(buffer.rope());
        let mut matches = self.cursor.matches(&self.query, tree.root_node(), provider);

        while let Some(m) = matches.next() {
            for capture in m.captures {
                let node = capture.node;
                self.captures.push(Capture {
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                    pattern_index: m.pattern_index,
                    metadata: self.capture_metadata[capture.index as usize],
                });
            }
        }
        paint::sort_captures(&mut self.captures);
    }

    /// Fill `out` with unstyled tokens, for when there is no parse tree.
    fn push_default_tokens(&self, buffer: &Buffer, lines: Range<usize>, out: &mut Vec<LineTokens>) {
        let default = self.theme.default_metadata();
        for line in lines.start..lines.end.min(buffer.len_lines()) {
            let len = buffer.line_len(line) as u32;
            out.push(if len == 0 {
                LineTokens::empty()
            } else {
                LineTokens::single(len, default)
            });
        }
    }
}

impl Highlighter for TreeSitterHighlighter {
    fn reset(&mut self, buffer: &Buffer) {
        self.tree = rope_input::parse(&mut self.parser, buffer, None);
    }

    fn apply_change(&mut self, buffer: &Buffer, change: &Change) {
        if self.tree.is_none() {
            self.reset(buffer);
            return;
        }
        self.edit_tree(buffer, change);
        // Reparse against the edited tree: tree-sitter reuses every subtree the
        // edit did not touch, which is what makes this O(edit) not O(document).
        let old = self.tree.take();
        self.tree = rope_input::parse(&mut self.parser, buffer, old.as_ref());
        if self.tree.is_none() {
            // A failed parse would otherwise leave stale highlighting in place.
            self.tree = rope_input::parse(&mut self.parser, buffer, None);
        }
    }

    fn tokens(&mut self, buffer: &Buffer, lines: Range<usize>, out: &mut Vec<LineTokens>) {
        let end_line = lines.end.min(buffer.len_lines());
        if lines.start >= end_line {
            return;
        }
        if self.tree.is_none() {
            self.push_default_tokens(buffer, lines, out);
            return;
        }

        let rope = buffer.rope();
        let start_byte = rope.char_to_byte(buffer.line_start(lines.start));
        let last_line = end_line - 1;
        let end_byte = rope.char_to_byte(buffer.line_end(last_line));

        self.collect_captures(buffer, start_byte..end_byte);

        // Move the buffers out so `self` is not borrowed twice; put them back
        // afterwards to keep reusing their allocations.
        let captures = std::mem::take(&mut self.captures);
        let mut scratch = std::mem::take(&mut self.scratch);
        paint::paint_lines(
            buffer,
            lines.start..end_line,
            &captures,
            self.theme.default_metadata(),
            &mut scratch,
            out,
        );
        self.captures = captures;
        self.scratch = scratch;
    }

    fn theme(&self) -> &Theme {
        &self.theme
    }
}
