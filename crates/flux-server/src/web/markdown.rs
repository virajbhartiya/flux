//! Safe Markdown rendering for READMEs, review descriptions and comments.
//!
//! Repository content is untrusted: raw HTML is rendered as text, link and
//! image URLs are restricted to safe schemes, and relative links resolve
//! inside the repository (images to raw content, other links to the file
//! browser).

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

/// Where relative links in a document resolve.
pub struct LinkBase<'a> {
    /// `/repo/-/blob/rev` (no trailing slash).
    pub blob: &'a str,
    /// `/repo/-/raw/rev`.
    pub raw: &'a str,
    /// Directory of the document inside the repo ("" for the root).
    pub dir: &'a str,
}

fn safe_url(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    match lower.split_once(':') {
        // Anything with a scheme must be one of these.
        Some((scheme, _)) if !scheme.contains('/') && !scheme.contains('#') && !scheme.contains('?') => {
            matches!(scheme, "http" | "https" | "mailto")
        }
        _ => true,
    }
}

fn is_relative(url: &str) -> bool {
    !url.contains("://") && !url.starts_with('/') && !url.starts_with('#') && !url.starts_with("mailto:")
}

/// Normalize `dir/rel` (resolving `.` and `..`, never escaping the root).
fn join(dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    let (path, suffix) = match rel.find(['#', '?']) {
        Some(i) => (&rel[..i], &rel[i..]),
        None => (rel, ""),
    };
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("{}{suffix}", parts.join("/"))
}

pub fn render(text: &str, base: Option<&LinkBase<'_>>) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);
    let rewrite = |url: CowStr<'static>, image: bool| -> CowStr<'static> {
        if !safe_url(&url) {
            return CowStr::from("#");
        }
        match base {
            Some(b) if is_relative(&url) => {
                let target = join(b.dir, &url);
                CowStr::from(format!("{}/{}", if image { b.raw } else { b.blob }, target))
            }
            _ => url,
        }
    };
    let parser = Parser::new_ext(text, opts).map(|ev| match ev {
        // Never pass through raw HTML from documents.
        Event::Html(s) | Event::InlineHtml(s) => Event::Text(s),
        Event::Start(Tag::Link { link_type, dest_url, title, id }) => {
            let dest_url = rewrite(dest_url.into_static(), false);
            Event::Start(Tag::Link { link_type, dest_url, title, id })
        }
        Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
            Event::Start(Tag::Image { link_type, dest_url: rewrite(dest_url.into_static(), true), title, id })
        }
        Event::End(TagEnd::Link) => Event::End(TagEnd::Link),
        other => other,
    });
    let mut out = String::with_capacity(text.len() * 3 / 2);
    html::push_html(&mut out, parser);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html_and_bad_urls() {
        let out = render("<script>alert(1)</script>\n\n[x](javascript:alert(1)) [y](JaVaScRiPt:1)", None);
        assert!(!out.contains("<script>"), "{out}");
        assert!(out.contains("&lt;script&gt;"));
        assert!(!out.to_ascii_lowercase().contains("javascript:"), "{out}");
    }

    #[test]
    fn rewrites_relative_links() {
        let base = LinkBase { blob: "/r/-/blob/main", raw: "/r/-/raw/main", dir: "docs" };
        let out = render("![i](img/a.png) [d](../src/lib.rs#L3) [w](https://x.org)", Some(&base));
        assert!(out.contains("src=\"/r/-/raw/main/docs/img/a.png\""), "{out}");
        assert!(out.contains("href=\"/r/-/blob/main/src/lib.rs#L3\""), "{out}");
        assert!(out.contains("href=\"https://x.org\""));
    }
}
