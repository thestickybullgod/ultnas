//! What a pending edit changes, line by line, with invisible characters
//! shown as visible markers so the reader sees exactly what Ultnas sees.

use similar::{ChangeTag, TextDiff};
use ultnas_core::invisible;

/// Unchanged lines shown around each change.
const CONTEXT: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    /// Unchanged lines left out between two changes.
    Gap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    /// A marker standing in for an invisible character, like `⟨U+200B⟩`.
    pub invisible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    /// 1-based line number in the stable version.
    pub old_no: Option<usize>,
    /// 1-based line number in the pending version.
    pub new_no: Option<usize>,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Default)]
pub struct Diff {
    pub lines: Vec<Line>,
    pub added: usize,
    pub removed: usize,
}

/// The changes from `old` to `new`, with a few lines of context each.
pub fn diff(old: &str, new: &str) -> Diff {
    let text = TextDiff::from_lines(old, new);
    let mut out = Diff::default();
    for (i, group) in text.grouped_ops(CONTEXT).iter().enumerate() {
        if i > 0 {
            out.lines.push(Line {
                kind: LineKind::Gap,
                old_no: None,
                new_no: None,
                segments: vec![],
            });
        }
        for op in group {
            for change in text.iter_changes(op) {
                let kind = match change.tag() {
                    ChangeTag::Equal => LineKind::Context,
                    ChangeTag::Insert => {
                        out.added += 1;
                        LineKind::Added
                    }
                    ChangeTag::Delete => {
                        out.removed += 1;
                        LineKind::Removed
                    }
                };
                let value = change.value();
                let value = value.strip_suffix('\n').unwrap_or(value);
                out.lines.push(Line {
                    kind,
                    old_no: change.old_index().map(|n| n + 1),
                    new_no: change.new_index().map(|n| n + 1),
                    segments: segments(value),
                });
            }
        }
    }
    out
}

/// `line` split around its invisible characters, each replaced by a marker.
pub fn segments(line: &str) -> Vec<Segment> {
    let mut out = vec![];
    let mut last = 0;
    for f in invisible::scan(line) {
        if f.offset > last {
            out.push(Segment {
                text: line[last..f.offset].to_string(),
                invisible: false,
            });
        }
        out.push(Segment {
            text: format!("⟨U+{:04X}⟩", f.ch as u32),
            invisible: true,
        });
        last = f.offset + f.ch.len_utf8();
    }
    if last < line.len() || out.is_empty() {
        out.push(Segment {
            text: line[last..].to_string(),
            invisible: false,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(d: &Diff) -> Vec<LineKind> {
        d.lines.iter().map(|l| l.kind).collect()
    }

    #[test]
    fn identical_texts_have_no_changes() {
        let d = diff("a\nb\n", "a\nb\n");
        assert!(d.lines.is_empty());
        assert_eq!((d.added, d.removed), (0, 0));
    }

    #[test]
    fn a_changed_line_is_removed_then_added_with_numbers() {
        let d = diff("one\ntwo\nthree\n", "one\nTWO\nthree\n");
        use LineKind::*;
        assert_eq!(kinds(&d), [Context, Removed, Added, Context]);
        assert_eq!((d.added, d.removed), (1, 1));
        assert_eq!((d.lines[1].old_no, d.lines[1].new_no), (Some(2), None));
        assert_eq!((d.lines[2].old_no, d.lines[2].new_no), (None, Some(2)));
        assert_eq!(d.lines[2].segments[0].text, "TWO");
    }

    #[test]
    fn distant_changes_are_split_by_a_gap() {
        let old: String = (1..=20).map(|n| format!("{n}\n")).collect();
        let new = old
            .replacen("2\n", "two\n", 1)
            .replacen("19\n", "nineteen\n", 1);
        let d = diff(&old, &new);
        assert_eq!(
            d.lines.iter().filter(|l| l.kind == LineKind::Gap).count(),
            1
        );
    }

    #[test]
    fn invisible_characters_become_markers() {
        let s = segments("is_\u{200B}admin");
        assert_eq!(
            s,
            [
                Segment {
                    text: "is_".into(),
                    invisible: false
                },
                Segment {
                    text: "⟨U+200B⟩".into(),
                    invisible: true
                },
                Segment {
                    text: "admin".into(),
                    invisible: false
                },
            ]
        );
    }

    #[test]
    fn an_empty_line_is_one_empty_segment() {
        assert_eq!(
            segments(""),
            [Segment {
                text: String::new(),
                invisible: false
            }]
        );
    }
}
