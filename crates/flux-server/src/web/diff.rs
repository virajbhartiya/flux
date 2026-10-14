//! Line diffs for the web UI (unified hunks with context).

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Del,
}

#[derive(Debug, Clone)]
pub struct Line {
    pub kind: LineKind,
    /// 1-based line numbers on each side.
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, Default)]
pub struct FileDiff {
    pub hunks: Vec<Hunk>,
    pub added: u32,
    pub deleted: u32,
}

pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8192)].contains(&0) || std::str::from_utf8(data).is_err()
}

/// Unified diff of two texts with `context` lines around changes. Bounded
/// by a deadline so pathological inputs can't stall a page.
pub fn diff_text(old: &str, new: &str, context: usize) -> FileDiff {
    let diff = similar::TextDiff::configure()
        .algorithm(similar::Algorithm::Patience)
        .timeout(Duration::from_millis(250))
        .diff_lines(old, new);
    let mut out = FileDiff::default();
    for group in diff.grouped_ops(context) {
        let (Some(first), Some(_)) = (group.first(), group.last()) else { continue };
        let mut hunk = Hunk {
            old_start: first.old_range().start as u32 + 1,
            new_start: first.new_range().start as u32 + 1,
            lines: Vec::new(),
        };
        for op in &group {
            for change in diff.iter_changes(op) {
                let kind = match change.tag() {
                    similar::ChangeTag::Equal => LineKind::Context,
                    similar::ChangeTag::Insert => {
                        out.added += 1;
                        LineKind::Add
                    }
                    similar::ChangeTag::Delete => {
                        out.deleted += 1;
                        LineKind::Del
                    }
                };
                let text = change.value().trim_end_matches(['\n', '\r']).to_string();
                hunk.lines.push(Line {
                    kind,
                    old: change.old_index().map(|i| i as u32 + 1),
                    new: change.new_index().map(|i| i as u32 + 1),
                    text,
                });
            }
        }
        out.hunks.push(hunk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_and_counts() {
        let old = (1..=20).map(|i| format!("line {i}\n")).collect::<String>();
        let new = old.replace("line 10\n", "line ten\nextra\n");
        let d = diff_text(&old, &new, 3);
        assert_eq!((d.added, d.deleted), (2, 1));
        assert_eq!(d.hunks.len(), 1);
        assert_eq!(d.hunks[0].old_start, 7);
        assert_eq!(d.hunks[0].lines.len(), 3 + 1 + 2 + 3);
    }
}
