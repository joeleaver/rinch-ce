//! Theme: maps highlight scopes to colors, and owns the color palette that
//! [`ColorId`] indexes into.
//!
//! Scope matching follows the tree-sitter convention (the same one Helix and
//! nvim-treesitter use): capture names are dotted paths like
//! `function.builtin.static`, and resolution walks from most to least specific,
//! stripping one trailing component at a time until a rule matches. That is the
//! tree-sitter analogue of Monaco's TextMate scope selectors — Monaco needs full
//! selector syntax because TextMate grammars emit parent-scope stacks, whereas a
//! tree-sitter capture is already a single dotted name.

use crate::token::{ColorId, FontStyle, StandardTokenType, TokenMetadata};
use std::collections::HashMap;

/// A straight 8-bit-per-channel color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::new(r, g, b, 0xFF)
    }

    /// Parse `#rgb`, `#rrggbb`, or `#rrggbbaa`. The leading `#` is optional.
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.strip_prefix('#').unwrap_or(s);
        let nib = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
        let bytes = s.as_bytes();
        match bytes.len() {
            3 => {
                let (r, g, b) = (nib(bytes[0])?, nib(bytes[1])?, nib(bytes[2])?);
                // #abc expands to #aabbcc.
                Some(Self::rgb(r * 17, g * 17, b * 17))
            }
            6 | 8 => {
                let mut v = [0u8; 4];
                v[3] = 0xFF;
                for (i, chunk) in bytes.chunks(2).enumerate() {
                    v[i] = nib(chunk[0])? * 16 + nib(chunk[1])?;
                }
                Some(Self::new(v[0], v[1], v[2], v[3]))
            }
            _ => None,
        }
    }

    /// CSS color string, for the renderer's inline styles.
    pub fn to_css(self) -> String {
        if self.a == 0xFF {
            format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            format!(
                "rgba({},{},{},{:.3})",
                self.r,
                self.g,
                self.b,
                self.a as f32 / 255.0
            )
        }
    }
}

/// The presentation a theme rule assigns to a scope. `None` means "inherit",
/// which is what lets a rule set only italic without disturbing the color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub foreground: Option<Rgba>,
    pub background: Option<Rgba>,
    pub font_style: FontStyle,
}

impl Style {
    pub fn fg(color: Rgba) -> Self {
        Self {
            foreground: Some(color),
            ..Default::default()
        }
    }

    pub fn with_font_style(mut self, style: FontStyle) -> Self {
        self.font_style = style;
        self
    }

    pub fn with_background(mut self, color: Rgba) -> Self {
        self.background = Some(color);
        self
    }
}

/// A resolved theme: a color palette plus scope rules.
#[derive(Debug, Clone)]
pub struct Theme {
    /// Index 0 is [`ColorId::UNSET`] and is never a real color.
    palette: Vec<Rgba>,
    /// Rules keyed by exact scope name; prefix walking happens at resolve time.
    rules: HashMap<String, TokenMetadata>,
    default_metadata: TokenMetadata,
}

impl Theme {
    pub fn builder() -> ThemeBuilder {
        ThemeBuilder::new()
    }

    /// Look up the color behind a [`ColorId`]. Returns `None` for `UNSET` and
    /// for ids past the end of the palette.
    pub fn color(&self, id: ColorId) -> Option<Rgba> {
        if !id.is_set() {
            return None;
        }
        self.palette.get(id.0 as usize).copied()
    }

    pub fn palette(&self) -> &[Rgba] {
        &self.palette
    }

    pub fn default_metadata(&self) -> TokenMetadata {
        self.default_metadata
    }

    /// Resolve a dotted scope to packed metadata, walking from most to least
    /// specific: `function.builtin.static` tries that, then `function.builtin`,
    /// then `function`, then falls back to the theme default.
    pub fn resolve(&self, scope: &str) -> TokenMetadata {
        let mut candidate = scope;
        loop {
            if let Some(meta) = self.rules.get(candidate) {
                return *meta;
            }
            match candidate.rfind('.') {
                Some(dot) => candidate = &candidate[..dot],
                None => return self.default_metadata,
            }
        }
    }

    /// Whether any rule could match this scope. Useful for diagnostics about
    /// theme coverage; not needed on the rendering path.
    pub fn has_rule_for(&self, scope: &str) -> bool {
        let mut candidate = scope;
        loop {
            if self.rules.contains_key(candidate) {
                return true;
            }
            match candidate.rfind('.') {
                Some(dot) => candidate = &candidate[..dot],
                None => return false,
            }
        }
    }
}

/// Builds a [`Theme`], interning colors into the palette as rules are added.
#[derive(Debug, Default)]
pub struct ThemeBuilder {
    palette: Vec<Rgba>,
    interned: HashMap<Rgba, ColorId>,
    rules: HashMap<String, TokenMetadata>,
    default_foreground: Option<Rgba>,
}

impl ThemeBuilder {
    pub fn new() -> Self {
        Self {
            // Slot 0 is reserved for UNSET; a real color never lands there.
            palette: vec![Rgba::default()],
            ..Default::default()
        }
    }

    /// Intern a color, returning its palette index. Repeated colors share an id,
    /// which is what keeps the palette inside the 9 bits the foreground field has.
    pub fn intern(&mut self, color: Rgba) -> ColorId {
        if let Some(&id) = self.interned.get(&color) {
            return id;
        }
        let id = ColorId(self.palette.len() as u16);
        self.palette.push(color);
        self.interned.insert(color, id);
        id
    }

    pub fn default_foreground(mut self, color: Rgba) -> Self {
        self.default_foreground = Some(color);
        self
    }

    /// Add a rule. `scope` is a dotted tree-sitter capture name.
    pub fn rule(mut self, scope: impl Into<String>, style: Style) -> Self {
        let scope = scope.into();
        // A scope's `StandardTokenType` is derived from its root component, so a
        // theme author never has to declare it — and the editor's bracket
        // matching and word-suggest logic get it for free.
        let token_type = standard_token_type_for(&scope);
        let fg = style.foreground.map(|c| self.intern(c)).unwrap_or_default();
        let bg = style.background.map(|c| self.intern(c)).unwrap_or_default();
        let meta = TokenMetadata::new(0, token_type, style.font_style, fg, bg);
        self.rules.insert(scope, meta);
        self
    }

    pub fn build(mut self) -> Theme {
        let default_fg = self
            .default_foreground
            .map(|c| self.intern(c))
            .unwrap_or_default();
        Theme {
            palette: self.palette,
            rules: self.rules,
            default_metadata: TokenMetadata::new(
                0,
                StandardTokenType::Other,
                FontStyle::NONE,
                default_fg,
                ColorId::UNSET,
            ),
        }
    }
}

/// Map a scope's root component to Monaco's coarse token class.
fn standard_token_type_for(scope: &str) -> StandardTokenType {
    let root = scope.split('.').next().unwrap_or(scope);
    match root {
        "comment" => StandardTokenType::Comment,
        "string" => StandardTokenType::String,
        "regex" | "regexp" => StandardTokenType::RegEx,
        _ => StandardTokenType::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::builder()
            .default_foreground(Rgba::rgb(0xAB, 0xB2, 0xBF))
            .rule("keyword", Style::fg(Rgba::rgb(0xC6, 0x78, 0xDD)))
            .rule("function", Style::fg(Rgba::rgb(0x61, 0xAF, 0xEF)))
            .rule(
                "function.builtin",
                Style::fg(Rgba::rgb(0x56, 0xB6, 0xC2)).with_font_style(FontStyle::ITALIC),
            )
            .rule("comment", Style::fg(Rgba::rgb(0x5C, 0x63, 0x70)))
            .rule("string", Style::fg(Rgba::rgb(0x98, 0xC3, 0x79)))
            .build()
    }

    #[test]
    fn hex_parsing_handles_every_accepted_form() {
        assert_eq!(Rgba::from_hex("#abc"), Some(Rgba::rgb(0xAA, 0xBB, 0xCC)));
        assert_eq!(Rgba::from_hex("abc"), Some(Rgba::rgb(0xAA, 0xBB, 0xCC)));
        assert_eq!(
            Rgba::from_hex("#12345678"),
            Some(Rgba::new(0x12, 0x34, 0x56, 0x78))
        );
        assert_eq!(Rgba::from_hex("#ff0000"), Some(Rgba::rgb(255, 0, 0)));
        assert_eq!(Rgba::from_hex("#gg0000"), None, "non-hex digits rejected");
        assert_eq!(Rgba::from_hex("#12345"), None, "odd length rejected");
    }

    #[test]
    fn css_output_omits_alpha_when_opaque() {
        assert_eq!(Rgba::rgb(0x61, 0xAF, 0xEF).to_css(), "#61afef");
        assert!(
            Rgba::new(0x61, 0xAF, 0xEF, 0x80)
                .to_css()
                .starts_with("rgba(")
        );
    }

    #[test]
    fn exact_scope_matches_win() {
        let t = theme();
        let meta = t.resolve("function.builtin");
        assert!(meta.font_style().is_italic());
        assert_eq!(
            t.color(meta.foreground()),
            Some(Rgba::rgb(0x56, 0xB6, 0xC2))
        );
    }

    #[test]
    fn unmatched_scopes_fall_back_to_the_longest_matching_prefix() {
        let t = theme();
        // No rule for `function.method`, so `function` should win.
        let meta = t.resolve("function.method");
        assert_eq!(
            t.color(meta.foreground()),
            Some(Rgba::rgb(0x61, 0xAF, 0xEF))
        );
        assert!(!meta.font_style().is_italic());

        // Deeper: strips twice down to `function.builtin`.
        let deep = t.resolve("function.builtin.static.inline");
        assert_eq!(
            t.color(deep.foreground()),
            Some(Rgba::rgb(0x56, 0xB6, 0xC2))
        );
    }

    #[test]
    fn a_completely_unknown_scope_gets_the_theme_default() {
        let t = theme();
        let meta = t.resolve("no.such.scope");
        assert_eq!(meta, t.default_metadata());
        assert_eq!(
            t.color(meta.foreground()),
            Some(Rgba::rgb(0xAB, 0xB2, 0xBF))
        );
    }

    #[test]
    fn standard_token_type_is_derived_from_the_scope_root() {
        let t = theme();
        assert_eq!(
            t.resolve("comment").token_type(),
            StandardTokenType::Comment
        );
        assert_eq!(
            t.resolve("comment.line.double-slash").token_type(),
            StandardTokenType::Comment,
            "derived even when matched via prefix"
        );
        assert_eq!(t.resolve("string").token_type(), StandardTokenType::String);
        assert_eq!(t.resolve("keyword").token_type(), StandardTokenType::Other);
    }

    #[test]
    fn identical_colors_share_one_palette_slot() {
        let shared = Rgba::rgb(1, 2, 3);
        let t = Theme::builder()
            .rule("a", Style::fg(shared))
            .rule("b", Style::fg(shared))
            .rule("c", Style::fg(Rgba::rgb(9, 9, 9)))
            .build();
        // Slot 0 (unset) + two distinct colors.
        assert_eq!(t.palette().len(), 3);
        assert_eq!(t.resolve("a").foreground(), t.resolve("b").foreground());
        assert_ne!(t.resolve("a").foreground(), t.resolve("c").foreground());
    }

    #[test]
    fn color_id_zero_never_resolves_to_a_color() {
        let t = theme();
        assert_eq!(t.color(ColorId::UNSET), None);
        assert_eq!(
            t.color(ColorId(9_999)),
            None,
            "out of range is None, not a panic"
        );
    }

    #[test]
    fn has_rule_for_agrees_with_resolve() {
        let t = theme();
        assert!(t.has_rule_for("function.method"));
        assert!(!t.has_rule_for("no.such.scope"));
        assert_eq!(t.resolve("no.such.scope"), t.default_metadata());
    }
}
