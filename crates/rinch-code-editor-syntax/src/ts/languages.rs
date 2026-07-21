//! The bundled language registry.
//!
//! Each grammar sits behind its own `lang-*` cargo feature. Grammars are the
//! bulk of a web build's size — a few hundred KB gzipped each — so nothing is
//! implied by enabling `tree-sitter` alone. A consumer opts into exactly the
//! languages it ships.
//!
//! Every grammar here is verified to build for `wasm32-unknown-unknown`. Most
//! needed a one-line build-script fix to read the freestanding headers that
//! `tree-sitter-language` already publishes; see the workspace manifest's
//! `[patch.crates-io]` block.

use tree_sitter::Language;

/// A language the editor can highlight.
///
/// The grammar is a function pointer rather than a value because
/// [`Language`] cannot be constructed in a `const`.
#[derive(Clone, Copy)]
pub struct LanguageConfig {
    /// Stable identifier, e.g. `"rust"`. Used for language selection and as
    /// the key an editor persists.
    pub name: &'static str,
    /// Extensions this language claims, without the leading dot.
    pub file_extensions: &'static [&'static str],
    grammar: fn() -> Language,
    /// Contents of the grammar's `highlights.scm`.
    pub highlights: &'static str,
}

impl LanguageConfig {
    /// The tree-sitter grammar. Cheap, but not free — it crosses into C.
    pub fn grammar(&self) -> Language {
        (self.grammar)()
    }
}

impl std::fmt::Debug for LanguageConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanguageConfig")
            .field("name", &self.name)
            .field("file_extensions", &self.file_extensions)
            .finish_non_exhaustive()
    }
}

macro_rules! language {
    ($feature:literal, $name:literal, [$($ext:literal),* $(,)?], $lang:expr, $highlights:expr $(,)?) => {
        #[cfg(feature = $feature)]
        LanguageConfig {
            name: $name,
            file_extensions: &[$($ext),*],
            grammar: || $lang,
            highlights: $highlights,
        }
    };
}

/// Every language compiled into this build, in registration order.
pub static LANGUAGES: &[LanguageConfig] = &[
    language!(
        "lang-rust",
        "rust",
        ["rs"],
        tree_sitter_rust::LANGUAGE.into(),
        tree_sitter_rust::HIGHLIGHTS_QUERY,
    ),
    language!(
        "lang-typescript",
        "typescript",
        ["ts", "mts", "cts"],
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
    ),
    language!(
        "lang-typescript",
        "tsx",
        ["tsx"],
        tree_sitter_typescript::LANGUAGE_TSX.into(),
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
    ),
    // Note the singular constant name here; this grammar spells it differently
    // from every other one in the set.
    language!(
        "lang-javascript",
        "javascript",
        ["js", "mjs", "cjs", "jsx"],
        tree_sitter_javascript::LANGUAGE.into(),
        tree_sitter_javascript::HIGHLIGHT_QUERY,
    ),
    language!(
        "lang-html",
        "html",
        ["html", "htm"],
        tree_sitter_html::LANGUAGE.into(),
        tree_sitter_html::HIGHLIGHTS_QUERY,
    ),
    language!(
        "lang-css",
        "css",
        ["css"],
        tree_sitter_css::LANGUAGE.into(),
        tree_sitter_css::HIGHLIGHTS_QUERY,
    ),
    language!(
        "lang-json",
        "json",
        ["json", "jsonc"],
        tree_sitter_json::LANGUAGE.into(),
        tree_sitter_json::HIGHLIGHTS_QUERY,
    ),
    language!(
        "lang-toml",
        "toml",
        ["toml"],
        tree_sitter_toml_ng::LANGUAGE.into(),
        tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
    ),
    language!(
        "lang-yaml",
        "yaml",
        ["yaml", "yml"],
        tree_sitter_yaml::LANGUAGE.into(),
        tree_sitter_yaml::HIGHLIGHTS_QUERY,
    ),
    language!(
        "lang-markdown",
        "markdown",
        ["md", "markdown"],
        tree_sitter_md::LANGUAGE.into(),
        tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
    ),
];

/// Look up a language by its stable name.
pub fn by_name(name: &str) -> Option<&'static LanguageConfig> {
    LANGUAGES.iter().find(|l| l.name == name)
}

/// Look up a language by file extension, with or without a leading dot.
///
/// Matching is case-insensitive, since `README.MD` and `Main.RS` are ordinary
/// on case-insensitive filesystems.
pub fn by_extension(extension: &str) -> Option<&'static LanguageConfig> {
    let ext = extension.trim_start_matches('.').to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|l| l.file_extensions.iter().any(|e| *e == ext))
}

/// Look up a language from a file path or name.
pub fn by_path(path: &str) -> Option<&'static LanguageConfig> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e)?;
    by_extension(ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_grammar_compiles_its_highlight_query() {
        // Guards the pairing, not the grammar: a query written for a different
        // grammar version fails to compile, and this is where that surfaces.
        for lang in LANGUAGES {
            let grammar = lang.grammar();
            tree_sitter::Query::new(&grammar, lang.highlights).unwrap_or_else(|e| {
                panic!("{}: highlight query failed to compile: {e}", lang.name)
            });
        }
    }

    #[test]
    fn language_names_are_unique() {
        for (i, a) in LANGUAGES.iter().enumerate() {
            for b in &LANGUAGES[i + 1..] {
                assert_ne!(a.name, b.name, "duplicate language name");
            }
        }
    }

    #[test]
    fn file_extensions_are_claimed_by_exactly_one_language() {
        for (i, a) in LANGUAGES.iter().enumerate() {
            for b in &LANGUAGES[i + 1..] {
                for ext in a.file_extensions {
                    assert!(
                        !b.file_extensions.contains(ext),
                        "{} and {} both claim .{ext}",
                        a.name,
                        b.name
                    );
                }
            }
        }
    }

    #[test]
    #[cfg(feature = "lang-rust")]
    fn lookup_by_name_extension_and_path() {
        assert_eq!(by_name("rust").map(|l| l.name), Some("rust"));
        assert_eq!(by_extension("rs").map(|l| l.name), Some("rust"));
        assert_eq!(by_extension(".rs").map(|l| l.name), Some("rust"));
        assert_eq!(by_extension("RS").map(|l| l.name), Some("rust"));
        assert_eq!(
            by_path("/home/joe/src/main.rs").map(|l| l.name),
            Some("rust")
        );
        assert_eq!(by_path("C:\\src\\main.rs").map(|l| l.name), Some("rust"));
        assert_eq!(by_name("nonesuch").map(|l| l.name), None);
        assert_eq!(by_path("Makefile").map(|l| l.name), None, "no extension");
    }

    #[test]
    #[cfg(feature = "lang-typescript")]
    fn one_crate_can_register_more_than_one_language() {
        // tree-sitter-typescript ships both TS and TSX from a single crate.
        assert_eq!(by_extension("ts").map(|l| l.name), Some("typescript"));
        assert_eq!(by_extension("tsx").map(|l| l.name), Some("tsx"));
    }
}
