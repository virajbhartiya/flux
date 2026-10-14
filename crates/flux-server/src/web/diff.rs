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

/// Tokens for intra-line diffs: runs of word characters, or single others.
fn tokens(s: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        let word = c.is_alphanumeric() || c == '_';
        match (word, start) {
            (true, None) => start = Some(i),
            (true, Some(_)) => {}
            (false, st) => {
                if let Some(b) = st {
                    out.push((b, &s[b..i]));
                    start = None;
                }
                out.push((i, &s[i..i + c.len_utf8()]));
            }
        }
    }
    if let Some(b) = start {
        out.push((b, &s[b..]));
    }
    out
}

/// Byte ranges that differ between two versions of a line, or `None` when
/// the lines are too different for word emphasis to help.
pub fn word_ranges(a: &str, b: &str) -> Option<(Vec<(usize, usize)>, Vec<(usize, usize)>)> {
    if a.len() > 600 || b.len() > 600 {
        return None;
    }
    let ta = tokens(a);
    let tb = tokens(b);
    let sa: Vec<&str> = ta.iter().map(|t| t.1).collect();
    let sb: Vec<&str> = tb.iter().map(|t| t.1).collect();
    let ops = similar::capture_diff_slices(similar::Algorithm::Myers, &sa, &sb);
    let (mut ra, mut rb) = (Vec::new(), Vec::new());
    let mut same = 0usize;
    let span = |toks: &[(usize, &str)], i: usize, n: usize| (toks[i].0, toks[i + n - 1].0 + toks[i + n - 1].1.len());
    for op in ops {
        match op {
            similar::DiffOp::Equal { len, old_index, .. } => {
                same += ta[old_index..old_index + len].iter().map(|t| t.1.len()).sum::<usize>()
            }
            similar::DiffOp::Delete { old_index, old_len, .. } => ra.push(span(&ta, old_index, old_len)),
            similar::DiffOp::Insert { new_index, new_len, .. } => rb.push(span(&tb, new_index, new_len)),
            similar::DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                ra.push(span(&ta, old_index, old_len));
                rb.push(span(&tb, new_index, new_len));
            }
        }
    }
    // Mostly rewritten lines read better without emphasis.
    if same * 2 < a.len().max(b.len()) {
        return None;
    }
    Some((ra, rb))
}

/// Wrap byte ranges of the *text* of `html` in `<span class="wd">`, closing
/// and reopening around tags so nesting stays valid.
pub fn wrap_ranges(html: &str, ranges: &[(usize, usize)]) -> String {
    if ranges.is_empty() {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len() + ranges.len() * 32);
    let mut pos = 0usize; // offset in the plain text
    let mut open = false;
    let inside = |p: usize| ranges.iter().any(|(a, b)| p >= *a && p < *b);
    let b = html.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'<' {
            let end = html[i..].find('>').map(|e| i + e + 1).unwrap_or(b.len());
            if open {
                out.push_str("</span>");
            }
            out.push_str(&html[i..end]);
            if open {
                out.push_str("<span class=\"wd\">");
            }
            i = end;
            continue;
        }
        let (text, width) = if b[i] == b'&' {
            let end = html[i..].find(';').map(|e| i + e + 1).unwrap_or(i + 1);
            (&html[i..end], 1)
        } else {
            let c = html[i..].chars().next().unwrap();
            (&html[i..i + c.len_utf8()], c.len_utf8())
        };
        let want = inside(pos);
        if want && !open {
            out.push_str("<span class=\"wd\">");
            open = true;
        } else if !want && open {
            out.push_str("</span>");
            open = false;
        }
        out.push_str(text);
        pos += width;
        i += text.len();
    }
    if open {
        out.push_str("</span>");
    }
    out
}

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod word_tests {
    use super::*;

    #[test]
    fn word_emphasis_survives_markup() {
        let (a, b) = word_ranges("let x = foo(1);", "let x = bar(1);").unwrap();
        assert_eq!(a, vec![(8, 11)]);
        assert_eq!(b, vec![(8, 11)]);
        let html = "<span class=\"k\">let</span> x = bar(1&lt;2);";
        let out = wrap_ranges(html, &[(8, 11)]);
        assert_eq!(out, "<span class=\"k\">let</span> x = <span class=\"wd\">bar</span>(1&lt;2);");
        let crossing = wrap_ranges("<i>ab</i>cd", &[(1, 3)]);
        assert_eq!(crossing, "<i>a<span class=\"wd\">b</span></i><span class=\"wd\">c</span>d");
    }
}
