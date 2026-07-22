//! The theme and the choice of highlighter.
//!
//! [`make_highlighter`] is where a `language:` prop becomes a real syntax
//! backend. When the named grammar was compiled in — via a `lang-*` feature that
//! forwards down through the model to the syntax crate — it builds a
//! tree-sitter highlighter; otherwise, or when no language is set, it falls back
//! to plain text so the editor always opens. There is no runtime grammar
//! loading: the set of languages is a compile-time decision the consuming app
//! makes in its manifest.
//!
//! The theme is a placeholder VS Code Dark+ palette, hard-coded here. Theming
//! becomes data — loadable, swappable at runtime via `set_theme` — in a later
//! increment; the colours below are stand-ins, not the design.

use rinch_code_editor_model::{Highlighter, PlainHighlighter, Rgba, Style, Theme};

/// Parse one of the literal hex colours below. They are author-controlled
/// constants, so a malformed one is a bug to fail on, not a runtime condition.
fn hex(s: &str) -> Rgba {
    Rgba::from_hex(s).expect("theme colour literals are valid hex")
}

/// A placeholder dark theme in the spirit of VS Code's Dark+. Scope names are
/// tree-sitter capture names; resolution walks from most to least specific, so a
/// rule on `function` also styles `function.method` unless a more specific rule
/// exists. Only the common captures are covered — an unmatched scope falls back
/// to the default foreground, which is correct, just uncoloured.
pub(crate) fn dark_theme() -> Theme {
    Theme::builder()
        .default_foreground(hex("#d4d4d4"))
        // Keywords: plain keywords blue, control-flow purple (VS Code's split).
        .rule("keyword", Style::fg(hex("#569cd6")))
        .rule("keyword.control", Style::fg(hex("#c586c0")))
        .rule("keyword.controlflow", Style::fg(hex("#c586c0")))
        .rule("conditional", Style::fg(hex("#c586c0")))
        .rule("repeat", Style::fg(hex("#c586c0")))
        // Functions and macros: pale yellow.
        .rule("function", Style::fg(hex("#dcdcaa")))
        .rule("function.macro", Style::fg(hex("#dcdcaa")))
        .rule("constructor", Style::fg(hex("#4ec9b0")))
        // Types, namespaces: teal.
        .rule("type", Style::fg(hex("#4ec9b0")))
        .rule("type.builtin", Style::fg(hex("#4ec9b0")))
        .rule("namespace", Style::fg(hex("#4ec9b0")))
        .rule("module", Style::fg(hex("#4ec9b0")))
        // Literals.
        .rule("string", Style::fg(hex("#ce9178")))
        .rule("string.escape", Style::fg(hex("#d7ba7d")))
        .rule("escape", Style::fg(hex("#d7ba7d")))
        .rule("character", Style::fg(hex("#ce9178")))
        .rule("number", Style::fg(hex("#b5cea8")))
        .rule("float", Style::fg(hex("#b5cea8")))
        .rule("boolean", Style::fg(hex("#569cd6")))
        .rule("constant", Style::fg(hex("#4fc1ff")))
        .rule("constant.builtin", Style::fg(hex("#569cd6")))
        // Comments: green.
        .rule("comment", Style::fg(hex("#6a9955")))
        // Variables, fields, properties: light blue.
        .rule("variable", Style::fg(hex("#9cdcfe")))
        .rule("variable.builtin", Style::fg(hex("#569cd6")))
        .rule("variable.parameter", Style::fg(hex("#9cdcfe")))
        .rule("property", Style::fg(hex("#9cdcfe")))
        .rule("field", Style::fg(hex("#9cdcfe")))
        // Attributes / decorators, labels, tags.
        .rule("attribute", Style::fg(hex("#dcdcaa")))
        .rule("label", Style::fg(hex("#c8c8c8")))
        .rule("tag", Style::fg(hex("#569cd6")))
        // Operators and punctuation stay at the default foreground.
        .rule("operator", Style::fg(hex("#d4d4d4")))
        .rule("punctuation", Style::fg(hex("#d4d4d4")))
        .build()
}

/// Build the highlighter for `language`, or a plain-text one when the language
/// is unset or its grammar is not compiled in.
#[cfg(feature = "tree-sitter")]
pub(crate) fn make_highlighter(language: &str, theme: Theme) -> Box<dyn Highlighter> {
    use rinch_code_editor_model::{TreeSitterHighlighter, languages};

    if !language.is_empty() {
        match languages::by_name(language) {
            Some(config) => match TreeSitterHighlighter::for_language(config, theme.clone()) {
                Ok(h) => return Box::new(h),
                Err(e) => {
                    // A grammar/query mismatch is a packaging bug, not anything a
                    // user did; surface it and open in plain text rather than fail.
                    eprintln!(
                        "rinch-code-editor: highlighter for {language:?} failed: {e}; \
                         falling back to plain text"
                    );
                }
            },
            None => eprintln!(
                "rinch-code-editor: language {language:?} is not compiled in \
                 (missing its lang-* feature); falling back to plain text"
            ),
        }
    }
    Box::new(PlainHighlighter::new(theme))
}

/// Plain text only: no grammar was compiled in, so `language` cannot be honoured.
#[cfg(not(feature = "tree-sitter"))]
pub(crate) fn make_highlighter(_language: &str, theme: Theme) -> Box<dyn Highlighter> {
    Box::new(PlainHighlighter::new(theme))
}
