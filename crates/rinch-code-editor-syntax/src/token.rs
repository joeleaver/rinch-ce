//! Packed token metadata, mirroring Monaco's `EncodedTokenAttributes` layout.
//!
//! A highlighted token carries language, token class, font style, and colors.
//! Storing that as a struct per token would mean one allocation per token and a
//! pointer chase per glyph run; a 5,000-line file is easily 50,000 tokens. Monaco
//! packs all of it into a single `u32` and stores tokens as a flat array, and we
//! do the same.
//!
//! Colors are **palette indices**, not RGB. That is what keeps the record at 32
//! bits, and it makes switching themes an O(1) palette swap instead of a
//! re-tokenization of the whole document.

/// Bit layout, byte-for-byte identical to Monaco's `MetadataConsts`:
///
/// ```text
///    3322 2222 2222 1111 1111 1100 0000 0000
///    1098 7654 3210 9876 5432 1098 7654 3210
///    bbbb bbbb ffff ffff fFFF FBTT LLLL LLLL
///
///    L = LanguageId       (8 bits, offset 0)
///    T = StandardTokenType(2 bits, offset 8)
///    B = BalancedBrackets (1 bit,  offset 10)
///    F = FontStyle        (4 bits, offset 11)
///    f = Foreground ColorId (9 bits, offset 15)
///    b = Background ColorId (8 bits, offset 24)
/// ```
mod consts {
    pub const LANGUAGE_ID_OFFSET: u32 = 0;
    pub const TOKEN_TYPE_OFFSET: u32 = 8;
    pub const BALANCED_BRACKETS_OFFSET: u32 = 10;
    pub const FONT_STYLE_OFFSET: u32 = 11;
    pub const FOREGROUND_OFFSET: u32 = 15;
    pub const BACKGROUND_OFFSET: u32 = 24;

    pub const LANGUAGE_ID_BITS: u32 = 8;
    pub const TOKEN_TYPE_BITS: u32 = 2;
    pub const BALANCED_BRACKETS_BITS: u32 = 1;
    pub const FONT_STYLE_BITS: u32 = 4;
    pub const FOREGROUND_BITS: u32 = 9;
    pub const BACKGROUND_BITS: u32 = 8;

    pub const fn mask(offset: u32, bits: u32) -> u32 {
        ((1u32 << bits) - 1) << offset
    }
}

use consts::*;

/// Monaco's `StandardTokenType`. Deliberately coarse: it exists so the editor
/// can answer "am I inside a comment or a string?" for bracket matching,
/// auto-closing, and word-based suggestions — not to describe syntax in detail.
/// Fine-grained classification lives in the theme's scope mapping instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum StandardTokenType {
    #[default]
    Other = 0,
    Comment = 1,
    String = 2,
    RegEx = 3,
}

impl StandardTokenType {
    const fn from_bits(bits: u32) -> Self {
        match bits {
            1 => Self::Comment,
            2 => Self::String,
            3 => Self::RegEx,
            _ => Self::Other,
        }
    }
}

/// Font style flags. Matches Monaco's `FontStyle` bit values exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FontStyle(u32);

impl FontStyle {
    pub const NONE: Self = Self(0);
    pub const ITALIC: Self = Self(1);
    pub const BOLD: Self = Self(2);
    pub const UNDERLINE: Self = Self(4);
    pub const STRIKETHROUGH: Self = Self(8);

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self(bits & ((1 << FONT_STYLE_BITS) - 1))
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn is_italic(self) -> bool {
        self.contains(Self::ITALIC)
    }

    pub const fn is_bold(self) -> bool {
        self.contains(Self::BOLD)
    }

    pub const fn is_underline(self) -> bool {
        self.contains(Self::UNDERLINE)
    }

    pub const fn is_strikethrough(self) -> bool {
        self.contains(Self::STRIKETHROUGH)
    }
}

impl core::ops::BitOr for FontStyle {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// An index into the theme's color palette. `0` is reserved to mean "unset",
/// so a token that specifies no color inherits the editor's default — the same
/// convention Monaco uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct ColorId(pub u16);

impl ColorId {
    pub const UNSET: Self = Self(0);

    pub const fn is_set(self) -> bool {
        self.0 != 0
    }
}

/// All of a token's presentation, packed into one `u32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct TokenMetadata(u32);

impl TokenMetadata {
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Build packed metadata. Every field is masked to its width, so an
    /// out-of-range value can never corrupt a neighbouring field — it saturates
    /// within its own bits instead.
    pub const fn new(
        language_id: u16,
        token_type: StandardTokenType,
        font_style: FontStyle,
        foreground: ColorId,
        background: ColorId,
    ) -> Self {
        let raw = ((language_id as u32) << LANGUAGE_ID_OFFSET
            & mask(LANGUAGE_ID_OFFSET, LANGUAGE_ID_BITS))
            | ((token_type as u32) << TOKEN_TYPE_OFFSET & mask(TOKEN_TYPE_OFFSET, TOKEN_TYPE_BITS))
            | (font_style.0 << FONT_STYLE_OFFSET & mask(FONT_STYLE_OFFSET, FONT_STYLE_BITS))
            | ((foreground.0 as u32) << FOREGROUND_OFFSET
                & mask(FOREGROUND_OFFSET, FOREGROUND_BITS))
            | ((background.0 as u32) << BACKGROUND_OFFSET
                & mask(BACKGROUND_OFFSET, BACKGROUND_BITS));
        Self(raw)
    }

    pub const fn language_id(self) -> u16 {
        ((self.0 & mask(LANGUAGE_ID_OFFSET, LANGUAGE_ID_BITS)) >> LANGUAGE_ID_OFFSET) as u16
    }

    pub const fn token_type(self) -> StandardTokenType {
        StandardTokenType::from_bits(
            (self.0 & mask(TOKEN_TYPE_OFFSET, TOKEN_TYPE_BITS)) >> TOKEN_TYPE_OFFSET,
        )
    }

    pub const fn font_style(self) -> FontStyle {
        FontStyle((self.0 & mask(FONT_STYLE_OFFSET, FONT_STYLE_BITS)) >> FONT_STYLE_OFFSET)
    }

    pub const fn foreground(self) -> ColorId {
        ColorId(((self.0 & mask(FOREGROUND_OFFSET, FOREGROUND_BITS)) >> FOREGROUND_OFFSET) as u16)
    }

    pub const fn background(self) -> ColorId {
        ColorId(((self.0 & mask(BACKGROUND_OFFSET, BACKGROUND_BITS)) >> BACKGROUND_OFFSET) as u16)
    }

    /// Whether this token participates in bracket-pair colorization.
    pub const fn has_balanced_brackets(self) -> bool {
        self.0 & mask(BALANCED_BRACKETS_OFFSET, BALANCED_BRACKETS_BITS) != 0
    }

    pub const fn with_balanced_brackets(self, value: bool) -> Self {
        let m = mask(BALANCED_BRACKETS_OFFSET, BALANCED_BRACKETS_BITS);
        Self(if value { self.0 | m } else { self.0 & !m })
    }

    /// Overlay `other`'s set fields onto `self`. Used to layer a semantic-token
    /// style over the syntactic one: only fields the overlay actually specifies
    /// win, so a semantic token that sets just italic keeps the syntax color.
    pub fn overlay(self, other: TokenMetadata) -> Self {
        let mut result = self;
        if other.foreground().is_set() {
            result = result.with_foreground(other.foreground());
        }
        if other.background().is_set() {
            result = result.with_background(other.background());
        }
        if other.font_style() != FontStyle::NONE {
            result = result.with_font_style(other.font_style());
        }
        result
    }

    pub const fn with_foreground(self, color: ColorId) -> Self {
        let m = mask(FOREGROUND_OFFSET, FOREGROUND_BITS);
        Self((self.0 & !m) | (((color.0 as u32) << FOREGROUND_OFFSET) & m))
    }

    pub const fn with_background(self, color: ColorId) -> Self {
        let m = mask(BACKGROUND_OFFSET, BACKGROUND_BITS);
        Self((self.0 & !m) | (((color.0 as u32) << BACKGROUND_OFFSET) & m))
    }

    pub const fn with_font_style(self, style: FontStyle) -> Self {
        let m = mask(FONT_STYLE_OFFSET, FONT_STYLE_BITS);
        Self((self.0 & !m) | ((style.0 << FONT_STYLE_OFFSET) & m))
    }

    pub const fn with_token_type(self, token_type: StandardTokenType) -> Self {
        let m = mask(TOKEN_TYPE_OFFSET, TOKEN_TYPE_BITS);
        Self((self.0 & !m) | (((token_type as u32) << TOKEN_TYPE_OFFSET) & m))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout must match Monaco's `MetadataConsts` literals exactly. These
    /// are copied from Monaco's source; if the derived masks ever drift from
    /// them, encoded tokens stop being interchangeable with Monaco's model.
    #[test]
    fn masks_match_monaco_exactly() {
        assert_eq!(
            mask(LANGUAGE_ID_OFFSET, LANGUAGE_ID_BITS),
            0b00000000_00000000_00000000_11111111
        );
        assert_eq!(
            mask(TOKEN_TYPE_OFFSET, TOKEN_TYPE_BITS),
            0b00000000_00000000_00000011_00000000
        );
        assert_eq!(
            mask(BALANCED_BRACKETS_OFFSET, BALANCED_BRACKETS_BITS),
            0b00000000_00000000_00000100_00000000
        );
        assert_eq!(
            mask(FONT_STYLE_OFFSET, FONT_STYLE_BITS),
            0b00000000_00000000_01111000_00000000
        );
        assert_eq!(
            mask(FOREGROUND_OFFSET, FOREGROUND_BITS),
            0b00000000_11111111_10000000_00000000
        );
        assert_eq!(
            mask(BACKGROUND_OFFSET, BACKGROUND_BITS),
            0b11111111_00000000_00000000_00000000
        );
    }

    #[test]
    fn every_bit_is_claimed_exactly_once() {
        let masks = [
            mask(LANGUAGE_ID_OFFSET, LANGUAGE_ID_BITS),
            mask(TOKEN_TYPE_OFFSET, TOKEN_TYPE_BITS),
            mask(BALANCED_BRACKETS_OFFSET, BALANCED_BRACKETS_BITS),
            mask(FONT_STYLE_OFFSET, FONT_STYLE_BITS),
            mask(FOREGROUND_OFFSET, FOREGROUND_BITS),
            mask(BACKGROUND_OFFSET, BACKGROUND_BITS),
        ];
        let mut seen = 0u32;
        for m in masks {
            assert_eq!(seen & m, 0, "fields overlap: {m:#034b}");
            seen |= m;
        }
        assert_eq!(seen, u32::MAX, "the layout leaves bits unused");
    }

    #[test]
    fn fields_roundtrip() {
        let m = TokenMetadata::new(
            42,
            StandardTokenType::String,
            FontStyle::ITALIC | FontStyle::BOLD,
            ColorId(300),
            ColorId(7),
        );
        assert_eq!(m.language_id(), 42);
        assert_eq!(m.token_type(), StandardTokenType::String);
        assert!(m.font_style().is_italic() && m.font_style().is_bold());
        assert!(!m.font_style().is_underline());
        assert_eq!(m.foreground(), ColorId(300));
        assert_eq!(m.background(), ColorId(7));
        assert!(!m.has_balanced_brackets());
    }

    #[test]
    fn extreme_field_values_do_not_bleed_into_neighbours() {
        // Foreground has 9 bits (max 511); background 8 (max 255).
        let m = TokenMetadata::new(
            u16::MAX,
            StandardTokenType::RegEx,
            FontStyle::from_bits_truncate(u32::MAX),
            ColorId(511),
            ColorId(255),
        );
        assert_eq!(m.foreground(), ColorId(511));
        assert_eq!(m.background(), ColorId(255));
        assert_eq!(m.token_type(), StandardTokenType::RegEx);
        assert_eq!(m.language_id(), 255, "8 bits, so u16::MAX truncates");
    }

    #[test]
    fn out_of_range_colors_truncate_without_corrupting_other_fields() {
        // 512 overflows foreground's 9 bits. It must not leak into background.
        let m = TokenMetadata::new(
            1,
            StandardTokenType::Comment,
            FontStyle::NONE,
            ColorId(512),
            ColorId(3),
        );
        assert_eq!(m.background(), ColorId(3), "background is intact");
        assert_eq!(m.language_id(), 1);
        assert_eq!(m.token_type(), StandardTokenType::Comment);
    }

    #[test]
    fn balanced_brackets_toggles_without_disturbing_anything_else() {
        let base = TokenMetadata::new(
            5,
            StandardTokenType::String,
            FontStyle::BOLD,
            ColorId(9),
            ColorId(2),
        );
        let on = base.with_balanced_brackets(true);
        assert!(on.has_balanced_brackets());
        assert_eq!(on.with_balanced_brackets(false), base);
        assert_eq!(on.foreground(), base.foreground());
        assert_eq!(on.font_style(), base.font_style());
    }

    #[test]
    fn overlay_only_applies_fields_the_overlay_actually_sets() {
        let syntax = TokenMetadata::new(
            1,
            StandardTokenType::Other,
            FontStyle::NONE,
            ColorId(10),
            ColorId::UNSET,
        );
        // A semantic token that specifies only italic.
        let semantic = TokenMetadata::new(
            0,
            StandardTokenType::Other,
            FontStyle::ITALIC,
            ColorId::UNSET,
            ColorId::UNSET,
        );
        let merged = syntax.overlay(semantic);
        assert_eq!(merged.foreground(), ColorId(10), "syntax color survives");
        assert!(merged.font_style().is_italic(), "semantic style applies");
    }

    #[test]
    fn unset_color_is_zero_so_default_metadata_inherits() {
        let m = TokenMetadata::default();
        assert!(!m.foreground().is_set());
        assert!(!m.background().is_set());
        assert_eq!(m.font_style(), FontStyle::NONE);
        assert_eq!(m.token_type(), StandardTokenType::Other);
    }
}
