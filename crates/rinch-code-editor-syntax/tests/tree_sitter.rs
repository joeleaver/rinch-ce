#![cfg(feature = "lang-rust")]
//! Integration tests for the tree-sitter backend, against a real grammar.
//!
//! A stub grammar would not exercise the things that actually break: capture
//! precedence between nested nodes, captures spanning lines, and whether an
//! incrementally edited tree agrees with a freshly parsed one.

use rinch_code_editor_buffer::{Buffer, Change, Edit};
use rinch_code_editor_syntax::{
    FontStyle, Highlighter, LineTokens, Rgba, Style, Theme, TreeSitterHighlighter, languages,
};

fn theme() -> Theme {
    Theme::builder()
        .default_foreground(Rgba::rgb(0xAB, 0xB2, 0xBF))
        .rule("keyword", Style::fg(Rgba::rgb(0xC6, 0x78, 0xDD)))
        .rule("function", Style::fg(Rgba::rgb(0x61, 0xAF, 0xEF)))
        .rule("type", Style::fg(Rgba::rgb(0xE5, 0xC0, 0x7B)))
        .rule("string", Style::fg(Rgba::rgb(0x98, 0xC3, 0x79)))
        .rule(
            "comment",
            Style::fg(Rgba::rgb(0x5C, 0x63, 0x70)).with_font_style(FontStyle::ITALIC),
        )
        .build()
}

fn highlighter() -> TreeSitterHighlighter {
    let rust = languages::by_name("rust").expect("lang-rust feature is on");
    TreeSitterHighlighter::for_language(rust, theme())
        .expect("rust grammar and highlight query should be valid")
}

fn tokens_of(h: &mut TreeSitterHighlighter, buffer: &Buffer) -> Vec<LineTokens> {
    let mut out = Vec::new();
    h.tokens(buffer, 0..buffer.len_lines(), &mut out);
    out
}

/// The styled substrings of a line, as `(text, foreground)` pairs.
fn styled(
    h: &TreeSitterHighlighter,
    buffer: &Buffer,
    line: usize,
    t: &LineTokens,
) -> Vec<(String, Option<Rgba>)> {
    let start = buffer.line_start(line);
    t.iter()
        .map(|span| {
            (
                buffer.slice(start + span.start as usize..start + span.end as usize),
                h.theme().color(span.metadata.foreground()),
            )
        })
        .collect()
}

#[test]
fn keywords_and_types_get_distinct_colors() {
    let buffer = Buffer::from_text("fn main() {}");
    let mut h = highlighter();
    h.reset(&buffer);
    let out = tokens_of(&mut h, &buffer);

    let spans = styled(&h, &buffer, 0, &out[0]);
    let keyword = spans
        .iter()
        .find(|(text, _)| text == "fn")
        .expect("`fn` should be a token");
    assert_eq!(
        keyword.1,
        Some(Rgba::rgb(0xC6, 0x78, 0xDD)),
        "`fn` is a keyword"
    );

    let default = h.theme().color(h.theme().default_metadata().foreground());
    assert_ne!(
        keyword.1, default,
        "keywords must not render as default text"
    );
}

#[test]
fn a_line_is_fully_covered_by_tokens() {
    let buffer = Buffer::from_text("let x: u32 = 42;\nfn f() {}");
    let mut h = highlighter();
    h.reset(&buffer);
    let out = tokens_of(&mut h, &buffer);

    for (line, tokens) in out.iter().enumerate() {
        assert_eq!(
            tokens.line_len() as usize,
            buffer.line_len(line),
            "line {line} is not fully tokenized"
        );
        // Spans must tile the line with no gap and no overlap.
        let mut expected_start = 0;
        for span in tokens.iter() {
            assert_eq!(span.start, expected_start, "gap or overlap on line {line}");
            expected_start = span.end;
        }
    }
}

#[test]
fn a_block_comment_spanning_lines_is_styled_on_every_line() {
    let buffer = Buffer::from_text("fn a() {}\n/* one\n   two\n   three */\nfn b() {}");
    let mut h = highlighter();
    h.reset(&buffer);
    let out = tokens_of(&mut h, &buffer);

    let comment = Rgba::rgb(0x5C, 0x63, 0x70);
    for (line, tokens) in out.iter().enumerate().take(4).skip(1) {
        let colors: Vec<_> = tokens
            .iter()
            .map(|s| h.theme().color(s.metadata.foreground()))
            .collect();
        assert!(
            colors.contains(&Some(comment)),
            "line {line} of the block comment is not styled as a comment"
        );
        assert!(
            tokens.iter().all(|s| s.metadata.font_style().is_italic()),
            "the comment rule sets italic, so every span on line {line} should be italic"
        );
    }
}

#[test]
fn a_block_comment_is_styled_even_when_it_starts_above_the_viewport() {
    // The regression this guards: querying only the visible lines must still
    // pick up a capture that began far earlier in the file.
    let mut text = String::from("/* opening\n");
    for i in 0..200 {
        text.push_str(&format!("   still a comment {i}\n"));
    }
    text.push_str("*/\nfn after() {}");
    let buffer = Buffer::from_text(&text);

    let mut h = highlighter();
    h.reset(&buffer);

    // Ask only for lines deep inside the comment.
    let mut out = Vec::new();
    h.tokens(&buffer, 150..152, &mut out);
    assert_eq!(out.len(), 2);

    let comment = Rgba::rgb(0x5C, 0x63, 0x70);
    for (i, tokens) in out.iter().enumerate() {
        let colors: Vec<_> = tokens
            .iter()
            .map(|s| h.theme().color(s.metadata.foreground()))
            .collect();
        assert!(
            colors.contains(&Some(comment)),
            "viewport line {i} lost its comment styling"
        );
    }
}

#[test]
fn incremental_reparse_matches_a_full_reparse() {
    // The property that validates `edit_tree`: after applying edits
    // incrementally, the tokens must be identical to parsing from scratch.
    let source = "fn main() {\n    let x = 1;\n    println!(\"hi\");\n}\n";

    let edits: Vec<Vec<Edit>> = vec![
        vec![Edit::insert(0, "// leading comment\n")],
        vec![Edit::insert(12, "pub ")],
        vec![Edit::replace(0..2, "/*")],
        vec![Edit::delete(5..9)],
        // Multi-cursor: three insertions at once.
        vec![
            Edit::insert(3, "A"),
            Edit::insert(10, "B"),
            Edit::insert(20, "C"),
        ],
        // An edit introducing newlines, which moves every later Point.
        vec![Edit::insert(15, "\n\nfn injected() {}\n")],
        // A multi-line deletion.
        vec![Edit::delete(2..25)],
    ];

    let mut buffer = Buffer::from_text(source);
    let mut incremental = highlighter();
    incremental.reset(&buffer);

    for (step, edit_set) in edits.into_iter().enumerate() {
        let change = Change::apply(&mut buffer, edit_set);
        incremental.apply_change(&buffer, &change);

        let mut fresh = highlighter();
        fresh.reset(&buffer);

        let got = tokens_of(&mut incremental, &buffer);
        let want = tokens_of(&mut fresh, &buffer);
        assert_eq!(
            got,
            want,
            "step {step}: incremental tokens diverged from a full reparse\ntext was:\n{}",
            buffer.text()
        );
    }
}

#[test]
fn incremental_reparse_survives_typing_character_by_character() {
    let mut buffer = Buffer::from_text("fn main() {}");
    let mut incremental = highlighter();
    incremental.reset(&buffer);

    // Type a whole line one character at a time, the common real-world case.
    let typed = "\n// a comment with \"quotes\" and 'chars'\nstruct S { f: u8 }";
    let start = buffer.len_chars();
    for (at, ch) in (start..).zip(typed.chars()) {
        let mut s = [0u8; 4];
        let change = Change::apply(
            &mut buffer,
            vec![Edit::insert(at, &*ch.encode_utf8(&mut s))],
        );
        incremental.apply_change(&buffer, &change);
    }

    let mut fresh = highlighter();
    fresh.reset(&buffer);
    assert_eq!(
        tokens_of(&mut incremental, &buffer),
        tokens_of(&mut fresh, &buffer),
        "character-by-character typing drifted from a full reparse"
    );
}

#[test]
fn multibyte_edits_keep_the_tree_in_sync() {
    // Byte and char offsets diverge here, so a confusion between them shows up
    // as a mismatch against the full reparse.
    let mut buffer = Buffer::from_text("fn main() {\n    let s = \"\";\n}");
    let mut incremental = highlighter();
    incremental.reset(&buffer);

    for insert in ["é", "🦀", "日本語", "a"] {
        // Insert just after the opening quote each time. Locating it by the
        // single `"` keeps this stable as the string literal grows.
        let quote_byte = buffer
            .text()
            .find('"')
            .expect("the string literal is still there");
        let at = buffer.rope().byte_to_char(quote_byte) + 1;
        let change = Change::apply(&mut buffer, vec![Edit::insert(at, insert)]);
        incremental.apply_change(&buffer, &change);

        // Compare at every step, so a divergence names the insertion that caused it.
        let mut fresh = highlighter();
        fresh.reset(&buffer);
        assert_eq!(
            tokens_of(&mut incremental, &buffer),
            tokens_of(&mut fresh, &buffer),
            "inserting {insert:?} desynchronized the tree; text is now {:?}",
            buffer.text()
        );
    }

    let mut fresh = highlighter();
    fresh.reset(&buffer);
    assert_eq!(
        tokens_of(&mut incremental, &buffer),
        tokens_of(&mut fresh, &buffer)
    );
}

#[test]
fn requesting_a_viewport_yields_exactly_those_lines() {
    let text = (0..500)
        .map(|i| format!("fn f{i}() {{ let x = {i}; }}"))
        .collect::<Vec<_>>()
        .join("\n");
    let buffer = Buffer::from_text(&text);
    let mut h = highlighter();
    h.reset(&buffer);

    let mut out = Vec::new();
    h.tokens(&buffer, 100..110, &mut out);
    assert_eq!(out.len(), 10);

    // And they must match what a whole-document pass produces for those lines.
    let all = tokens_of(&mut h, &buffer);
    assert_eq!(out.as_slice(), &all[100..110]);
}

#[test]
fn changing_theme_restyles_without_reparsing() {
    let buffer = Buffer::from_text("fn main() { let x = 1; }");
    let mut h = highlighter();
    h.reset(&buffer);
    let before = tokens_of(&mut h, &buffer);
    let tree_before = h.tree().map(|t| t.root_node().to_sexp());

    h.set_theme(
        Theme::builder()
            .default_foreground(Rgba::rgb(0x11, 0x11, 0x11))
            .rule("keyword", Style::fg(Rgba::rgb(0xFF, 0x00, 0x00)))
            .build(),
    );
    let after = tokens_of(&mut h, &buffer);

    assert_eq!(
        tree_before,
        h.tree().map(|t| t.root_node().to_sexp()),
        "re-theming must not touch the parse tree"
    );
    assert_ne!(
        before, after,
        "the new theme should change the token colors"
    );
}

/// The wasm allocator canary.
///
/// tree-sitter's wasm bump allocator corrupts memory but only fails above
/// roughly 16 KB, so a small parse passes even when it is broken. This input is
/// deliberately far past that threshold. On native it is a plain large-input
/// test; on `wasm32-unknown-unknown` it is the check that
/// `TreeSitterHighlighter::new` installed the Rust allocator.
///
/// See `src/ts/alloc.rs` and tree-sitter issue #5530.
#[test]
fn parses_an_input_far_larger_than_the_wasm_allocator_threshold() {
    let mut text = String::with_capacity(128 * 1024);
    let mut line = 0;
    while text.len() < 128 * 1024 {
        text.push_str(&format!(
            "/// docs for item {line}\npub fn function_{line}(argument: &str) -> Result<Vec<String>, Error> {{\n    let value = format!(\"{{}}\", argument);\n    Ok(vec![value])\n}}\n"
        ));
        line += 1;
    }
    assert!(text.len() > 32 * 1024, "canary input must exceed 32 KB");

    let buffer = Buffer::from_text(&text);
    let mut h = highlighter();
    h.reset(&buffer);

    assert!(h.tree().is_some(), "a large input must still parse");

    // Exercise the tree rather than just building it: the corruption shows up
    // when the tree is walked, not only when it is created.
    let mut out = Vec::new();
    h.tokens(&buffer, 0..buffer.len_lines(), &mut out);
    assert_eq!(out.len(), buffer.len_lines());

    let keyword = Rgba::rgb(0xC6, 0x78, 0xDD);
    let found_keyword = out
        .iter()
        .flat_map(|t| t.iter())
        .any(|s| h.theme().color(s.metadata.foreground()) == Some(keyword));
    assert!(found_keyword, "a large file should still be highlighted");
}

/// Repeated large parses, to catch a leak or a corrupted allocator that only
/// manifests after the heap has been churned.
#[test]
fn repeated_large_reparses_stay_stable() {
    let unit = "pub fn f(x: u32) -> u32 { let y = x + 1; y }\n";
    let text = unit.repeat(1200); // comfortably over 32 KB
    assert!(text.len() > 32 * 1024);

    let mut buffer = Buffer::from_text(&text);
    let mut h = highlighter();
    h.reset(&buffer);
    let baseline = tokens_of(&mut h, &buffer);

    for i in 0..8 {
        let change = Change::apply(&mut buffer, vec![Edit::insert(0, "// x\n")]);
        h.apply_change(&buffer, &change);
        let change = Change::apply(&mut buffer, vec![Edit::delete(0..5)]);
        h.apply_change(&buffer, &change);

        assert_eq!(
            tokens_of(&mut h, &buffer),
            baseline,
            "round {i}: tokens drifted after an insert/delete cycle"
        );
    }
}
