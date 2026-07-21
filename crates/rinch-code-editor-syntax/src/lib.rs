//! Syntax highlighting for the rinch code editor.
//!
//! The token model is deliberately Monaco's: presentation is packed into a
//! single `u32` per token ([`TokenMetadata`]), tokens are stored as flat
//! per-line arrays ([`LineTokens`]), and colors are palette indices rather than
//! RGB so that changing theme is a palette swap rather than a re-tokenization.
//!
//! Highlighting is *derived state*. It is never stored in the buffer, never
//! enters the undo history, and never reaches the CRDT — it is recomputed from
//! the text and thrown away. That separation is why the editor can re-theme,
//! re-highlight, or change language without touching the document.
//!
//! # Backends
//!
//! [`Highlighter`] is the seam. The crate ships a plain-text implementation with
//! no dependencies; the tree-sitter backend lives behind the `tree-sitter`
//! feature so a consumer who does not want it links no C code at all.

mod highlighter;
mod line_tokens;
mod theme;
mod token;

#[cfg(feature = "tree-sitter")]
mod ts;

pub use highlighter::{Highlighter, PlainHighlighter};
pub use line_tokens::{LineTokens, LineTokensBuilder, TokenSpan};
pub use theme::{Rgba, Style, Theme, ThemeBuilder};
pub use token::{ColorId, FontStyle, StandardTokenType, TokenMetadata};

#[cfg(feature = "tree-sitter")]
pub use ts::languages::{self, LanguageConfig};
#[cfg(feature = "tree-sitter")]
pub use ts::{SyntaxError, TreeSitterHighlighter};
