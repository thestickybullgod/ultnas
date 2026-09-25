//! Invisible-character detection for tracked text files.
//!
//! ## What counts
//! Characters that render as nothing, or silently reorder the text around
//! them, so a write can change meaning without changing what a reader sees:
//! zero-width characters, bidi controls (the "Trojan Source" attack), tag
//! characters, fillers, and other Unicode format characters.
//!
//! ## Context-sensitive characters
//! Some have legitimate uses in non-ASCII text: ZWJ in emoji sequences, ZWNJ
//! in Persian and Indic scripts, variation selectors after emoji, LRM/RLM in
//! right-to-left text, tag characters in subdivision flags (🏴 + tags). These
//! are flagged only where they have no such use: with ASCII (or the edge of
//! the text) on *both* sides, looking past other invisible characters so an
//! attacker can't hide one ZWJ behind another.
//!
//! ## Introduced vs. pre-existing
//! A tracked file's baseline may already contain some of these (an emoji ZWJ
//! sequence, say). [`introduced`] reports only occurrences beyond the
//! baseline's count of each character, so a pre-existing one never makes an
//! unrelated edit look like an attack.

use std::{collections::HashMap, fmt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvisibleKind {
    ZeroWidth,
    Bidi,
    Tag,
    VariationSelector,
    Format,
    Separator,
}

impl InvisibleKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            InvisibleKind::ZeroWidth => "zero-width",
            InvisibleKind::Bidi => "bidi control",
            InvisibleKind::Tag => "tag",
            InvisibleKind::VariationSelector => "variation selector",
            InvisibleKind::Format => "format",
            InvisibleKind::Separator => "line/paragraph separator",
        }
    }
}

/// One flagged character in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub ch: char,
    pub kind: InvisibleKind,
    /// Byte offset of the character.
    pub offset: usize,
    /// 1-based line.
    pub line: usize,
    /// 1-based column, in characters.
    pub column: usize,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "U+{:04X} ({}) at {}:{}",
            self.ch as u32,
            self.kind.as_str(),
            self.line,
            self.column
        )
    }
}

/// How a new version of a tracked file relates to its baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// No invisible characters beyond the baseline's. An ordinary edit.
    Clean,
    /// The write added invisible characters.
    Introduced(Vec<Finding>),
    /// The new bytes aren't UTF-8, so they can't be checked (or a text file
    /// was overwritten with binary).
    NotText,
}

/// The kind of `c`, and whether it is context-sensitive (see module docs).
fn kind_of(c: char) -> Option<(InvisibleKind, bool)> {
    use InvisibleKind::*;
    Some(match c {
        '\u{200B}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}' | '\u{180E}' => (ZeroWidth, false),
        '\u{200C}' | '\u{200D}' => (ZeroWidth, true),
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => (Bidi, false),
        '\u{200E}' | '\u{200F}' | '\u{061C}' => (Bidi, true),
        '\u{E0001}' => (Tag, false),
        '\u{E0020}'..='\u{E007F}' => (Tag, true),
        '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}' => (VariationSelector, true),
        '\u{00AD}'
        | '\u{034F}'
        | '\u{115F}'
        | '\u{1160}'
        | '\u{17B4}'
        | '\u{17B5}'
        | '\u{3164}'
        | '\u{FFA0}'
        | '\u{206A}'..='\u{206F}'
        | '\u{FFF9}'..='\u{FFFB}'
        | '\u{1D173}'..='\u{1D17A}' => (Format, false),
        '\u{2028}' | '\u{2029}' => (Separator, false),
        _ => return None,
    })
}

const BLACK_FLAG: char = '\u{1F3F4}';
const KEYCAP: char = '\u{20E3}';

/// Every flagged character in `text`, in order.
pub fn scan(text: &str) -> Vec<Finding> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    // Nearest neighbour that isn't itself a candidate; `None` is the text edge.
    let visible = |from: usize, step: isize| -> Option<char> {
        let mut i = from as isize + step;
        while i >= 0 && (i as usize) < chars.len() {
            let c = chars[i as usize].1;
            if kind_of(c).is_none() {
                return Some(c);
            }
            i += step;
        }
        None
    };
    let ascii_or_edge = |c: Option<char>| c.is_none_or(|c| c.is_ascii());

    let mut out = vec![];
    let (mut line, mut column) = (1, 0);
    let mut in_flag_tags = false;
    for (i, &(offset, c)) in chars.iter().enumerate() {
        column += 1;
        if c == '\n' {
            (line, column) = (line + 1, 0);
        }
        let Some((kind, contextual)) = kind_of(c) else {
            in_flag_tags = c == BLACK_FLAG;
            continue;
        };
        let flagged = match c {
            // A byte-order mark is legitimate at the very start of a file.
            '\u{FEFF}' => offset != 0,
            // Subdivision flags: 🏴 followed by a run of tag characters.
            '\u{E0020}'..='\u{E007F}' => !in_flag_tags,
            // Keycap emoji: an ASCII digit, #, or * then U+FE0F then U+20E3.
            '\u{FE0F}' if chars.get(i + 1).map(|&(_, n)| n) == Some(KEYCAP) => false,
            _ if contextual => ascii_or_edge(visible(i, -1)) && ascii_or_edge(visible(i, 1)),
            _ => true,
        };
        if kind != InvisibleKind::Tag {
            in_flag_tags = false;
        }
        if flagged {
            out.push(Finding {
                ch: c,
                kind,
                offset,
                line,
                column,
            });
        }
    }
    out
}

/// Findings in `new` beyond what `baseline` already contained, per character.
pub fn introduced(baseline: &str, new: &str) -> Vec<Finding> {
    let mut allowance: HashMap<char, usize> = HashMap::new();
    for f in scan(baseline) {
        *allowance.entry(f.ch).or_default() += 1;
    }
    scan(new)
        .into_iter()
        .filter(|f| match allowance.get_mut(&f.ch) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .collect()
}

/// `new` with the characters it introduced over `baseline` removed.
pub fn strip_introduced(baseline: &str, new: &str) -> String {
    let drop: Vec<usize> = introduced(baseline, new).iter().map(|f| f.offset).collect();
    new.char_indices()
        .filter(|(i, _)| drop.binary_search(i).is_err())
        .map(|(_, c)| c)
        .collect()
}

/// Classify a write to a tracked text file against its baseline.
pub fn classify_change(baseline: &[u8], new: &[u8]) -> Change {
    let Ok(new) = std::str::from_utf8(new) else {
        return Change::NotText;
    };
    let baseline = String::from_utf8_lossy(baseline);
    let found = introduced(&baseline, new);
    if found.is_empty() {
        Change::Clean
    } else {
        Change::Introduced(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flagged(text: &str) -> Vec<char> {
        scan(text).into_iter().map(|f| f.ch).collect()
    }

    #[test]
    fn flags_zero_width_and_bidi_in_ascii() {
        assert_eq!(flagged("pass\u{200B}word"), vec!['\u{200B}']);
        assert_eq!(
            flagged("if admin \u{202E}\u{2066}// check"),
            vec!['\u{202E}', '\u{2066}']
        );
        assert!(flagged("plain ascii text\n").is_empty());
    }

    #[test]
    fn reports_line_and_column() {
        let f = &scan("ab\ncd\u{200B}e")[0];
        assert_eq!((f.line, f.column, f.offset), (2, 3, 5));
        assert_eq!(f.to_string(), "U+200B (zero-width) at 2:3");
    }

    #[test]
    fn contextual_characters_allowed_in_non_ascii_text() {
        // Family emoji (ZWJ), Persian ZWNJ, heart + VS16, keycap one, England flag.
        let legit = "👨\u{200D}👩 می\u{200C}خواهم ❤\u{FE0F} 1\u{FE0F}\u{20E3} \
                     \u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";
        assert!(flagged(legit).is_empty(), "{:?}", scan(legit));
    }

    #[test]
    fn contextual_characters_flagged_between_ascii() {
        assert_eq!(flagged("ad\u{200D}min"), vec!['\u{200D}']);
        assert_eq!(flagged("x\u{E0041}y"), vec!['\u{E0041}']);
        // Stacking invisibles doesn't make them look "non-ASCII adjacent".
        assert_eq!(flagged("a\u{200D}\u{200D}b").len(), 2);
    }

    #[test]
    fn leading_bom_allowed_elsewhere_flagged() {
        assert!(flagged("\u{FEFF}hello").is_empty());
        assert_eq!(flagged("hel\u{FEFF}lo"), vec!['\u{FEFF}']);
    }

    #[test]
    fn introduced_ignores_characters_already_in_baseline() {
        let baseline = "a\u{200B}b";
        assert!(introduced(baseline, "a\u{200B}b and more").is_empty());
        let found = introduced(baseline, "a\u{200B}b\u{200B}");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].offset, 5);
    }

    #[test]
    fn strip_introduced_restores_pure_insertion() {
        let baseline = "let is_admin = false;";
        let attacked = "let is_\u{200B}admin = \u{202E}false;";
        assert_eq!(strip_introduced(baseline, attacked), baseline);
    }

    #[test]
    fn classify_change_cases() {
        assert_eq!(classify_change(b"abc", b"abcd"), Change::Clean);
        assert!(matches!(
            classify_change(b"abc", "ab\u{200B}c".as_bytes()),
            Change::Introduced(_)
        ));
        assert_eq!(classify_change(b"abc", &[0xff, 0xfe]), Change::NotText);
    }
}
