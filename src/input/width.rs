//! How many terminal columns a string occupies.
//!
//! The prompt redraws its line and then parks the cursor by moving it left by
//! the width of the text after it, which is only correct if the width computed
//! here matches what the terminal did. Two things break the naive
//! `chars().count()`:
//!
//! * combining marks (Thai vowel/tone marks, Latin diacritics) draw on top of
//!   the previous cell — `ส` + `ะ`-style marks occupy one column, not two
//! * CJK/Hiragana/Katakana and most emoji occupy two
//!
//! The ranges below are the small set that actually shows up in terminal text;
//! anything not listed counts as one column, which is the same fallback
//! `wcwidth(3)` gives.

/// Columns `c` occupies: 0 for combining marks and variation selectors, 2 for
/// wide/emoji ranges, 1 otherwise.
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    if is_zero_width(cp) {
        return 0;
    }
    if is_wide(cp) {
        return 2;
    }
    1
}

/// Columns `s` occupies.
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn is_zero_width(cp: u32) -> bool {
    matches!(cp,
        // Combining diacritical marks, supplement, and the Asian script
        // combining ranges (Thai, Myanmar, Khmer, Devanagari, ...).
        0x0300..=0x036F
        | 0x0483..=0x0489
        | 0x0591..=0x05BD
        | 0x0610..=0x061A
        | 0x064B..=0x065F
        | 0x0670
        | 0x06D6..=0x06DC
        | 0x0730..=0x074A
        | 0x07A6..=0x07B0
        | 0x0900..=0x0903
        | 0x093A..=0x094F
        | 0x0951..=0x0957
        | 0x0E31
        | 0x0E34..=0x0E3A
        | 0x0E47..=0x0E4E
        | 0x0EB1
        | 0x0EB4..=0x0EBC
        | 0x0EC8..=0x0ECD
        | 0x102B..=0x103E
        | 0x1056..=0x1059
        | 0x135D..=0x135F
        | 0x1712..=0x1714
        | 0x17B4..=0x17D3
        | 0x180B..=0x180E
        | 0x200B..=0x200F
        | 0x20D0..=0x20F0
        | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F
        // (0x200D, the zero-width joiner multi-codepoint emoji need, is inside
        // the 0x200B..=0x200F range above.)
    )
}

fn is_wide(cp: u32) -> bool {
    matches!(cp,
        0x1100..=0x115F
        | 0x231A..=0x231B
        | 0x2329..=0x232A
        | 0x23E9..=0x23EC
        | 0x25FD..=0x25FE
        | 0x2614..=0x2615
        | 0x2648..=0x2653
        | 0x267F
        | 0x2693
        | 0x26A1
        | 0x26AA..=0x26AB
        | 0x26BD..=0x26BE
        | 0x26C4..=0x26C5
        | 0x2705
        | 0x270A..=0x270B
        | 0x2728
        | 0x274C
        | 0x274E
        | 0x2753..=0x2755
        | 0x2757
        | 0x2795..=0x2797
        | 0x27B0
        | 0x27BF
        | 0x2B1B..=0x2B1C
        | 0x2B50
        | 0x2B55
        | 0x2E80..=0x2E99
        | 0x2E9B..=0x2EF3
        | 0x2F00..=0x2FD5
        | 0x2FF0..=0x2FFB
        | 0x3000..=0x303E
        | 0x3041..=0x3096
        | 0x3099..=0x30FF
        | 0x3105..=0x312F
        | 0x3131..=0x318E
        | 0x3190..=0x31E3
        | 0x31F0..=0x321E
        | 0x3220..=0x3247
        | 0x3250..=0x4DBF
        | 0x4E00..=0xA48C
        | 0xA490..=0xA4C6
        | 0xA960..=0xA97C
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE52
        | 0xFE54..=0xFE66
        | 0xFE68..=0xFE6B
        | 0xFF01..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x16FE0..=0x16FE4
        | 0x17000..=0x18D08
        | 0x1B000..=0x1B152
        | 0x1F004
        | 0x1F0CF
        | 0x1F18E
        | 0x1F191..=0x1F19A
        | 0x1F200..=0x1F320
        | 0x1F32D..=0x1F335
        | 0x1F337..=0x1F37C
        | 0x1F37E..=0x1F393
        | 0x1F3A0..=0x1F3CA
        | 0x1F3CF..=0x1F3D3
        | 0x1F3E0..=0x1F3F0
        | 0x1F3F4
        | 0x1F3F8..=0x1F43E
        | 0x1F440
        | 0x1F442..=0x1F4FC
        | 0x1F4FF..=0x1F53D
        | 0x1F54B..=0x1F54E
        | 0x1F550..=0x1F567
        | 0x1F57A
        | 0x1F595..=0x1F596
        | 0x1F5A4
        | 0x1F5FB..=0x1F64F
        | 0x1F680..=0x1F6C5
        | 0x1F6CC
        | 0x1F6D0..=0x1F6D2
        | 0x1F6EB..=0x1F6EC
        | 0x1F6F4..=0x1F6FC
        | 0x1F7E0..=0x1F7EB
        | 0x1F90C..=0x1F93A
        | 0x1F93C..=0x1F945
        | 0x1F947..=0x1F978
        | 0x1F97A..=0x1F9CB
        | 0x1F9CD..=0x1F9FF
        | 0x1FA70..=0x1FAFF
        | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_one_column() {
        assert_eq!(display_width("hello"), 5);
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn thai_combining_marks_do_not_add_columns() {
        // ส (base) + ะ (combining vowel) = one cell; the sara aa in "ไทย"
        // combines as well.
        assert_eq!(char_width('ส'), 1);
        assert_eq!(char_width('\u{0E31}'), 0); // mai han akat
        assert_eq!(char_width('\u{0E34}'), 0); // sara i
        assert_eq!(char_width('\u{0E48}'), 0); // mai ek (tone)
        assert_eq!(display_width("ส\u{0E31}ง"), 2);
    }

    #[test]
    fn cjk_hiragana_and_emoji_are_two_columns() {
        assert_eq!(char_width('\u{4F60}'), 2); // CJK "you"
        assert_eq!(char_width('\u{3042}'), 2); // hiragana A
        assert_eq!(char_width('\u{1F600}'), 2); // emoji
        assert_eq!(display_width("hi \u{4F60}\u{4F60}"), 7);
    }

    #[test]
    fn symbols_outside_the_wide_ranges_stay_one_column() {
        assert_eq!(char_width('/'), 1);
        assert_eq!(char_width('*'), 1);
        assert_eq!(char_width('\u{2605}'), 1); // BLACK STAR
        assert_eq!(char_width('\u{2500}'), 1); // box drawing
    }

    #[test]
    fn a_thai_sentence_measures_cells_not_codepoints() {
        let s = "\u{0E0A}\u{0E27}\u{0E22}\u{0E41}\u{0E01}\u{0E47}\u{0E1A}\u{0E31}\u{0E01}\u{0E2B}\u{0E19}\u{0E49}\u{0E2D}\u{0E22}";
        assert!(
            s.chars().count() > display_width(s),
            "combining marks must not add columns"
        );
    }
}
