//! Syntax highlighting with syntect, rendered as per-line HTML (each line is
//! self-contained, so lines can sit in table rows with anchors). Results are
//! cached by content id: content is immutable, so a file is highlighted once.

use std::sync::{Arc, OnceLock};

use flux_core::ContentId;
use syntect::html::ClassStyle;
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

/// Files larger than this are shown without highlighting.
pub const MAX_HIGHLIGHT_BYTES: usize = 1 << 20;
const CLASS_PREFIX: &str = "hl-";

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();

/// Load grammars in the background at startup so the first page is fast.
pub fn warm_up() {
    std::thread::spawn(|| {
        syntaxes();
    });
}

fn syntaxes() -> &'static SyntaxSet {
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

pub struct Highlighter {
    cache: moka::sync::Cache<(ContentId, &'static str), Arc<Vec<String>>>,
}

impl Default for Highlighter {
    fn default() -> Self {
        Self {
            cache: moka::sync::Cache::builder()
                .weigher(|_, v: &Arc<Vec<String>>| v.iter().map(|l| l.len() + 24).sum::<usize>().max(64) as u32)
                .max_capacity(256 << 20)
                .build(),
        }
    }
}

fn find_syntax<'a>(ss: &'a SyntaxSet, path: &str, first_line: &str) -> Option<&'a SyntaxReference> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let by_name = match name {
        "Makefile" | "makefile" | "GNUmakefile" => ss.find_syntax_by_extension("make"),
        "Dockerfile" => ss.find_syntax_by_name("Dockerfile"),
        "CMakeLists.txt" => ss.find_syntax_by_name("CMake"),
        "Cargo.lock" => ss.find_syntax_by_extension("toml"),
        _ => None,
    };
    by_name
        .or_else(|| {
            let ext = name.rsplit_once('.').map(|(_, e)| e)?;
            let ext = match ext {
                "ts" | "tsx" | "mts" | "cts" => "js",
                "jsx" | "mjs" | "cjs" => "js",
                "toml" | "lock" => "toml",
                "yml" => "yaml",
                "h" | "hpp" | "hh" | "cc" | "cxx" => "cpp",
                "kts" => "kt",
                e => e,
            };
            ss.find_syntax_by_extension(ext)
        })
        .or_else(|| ss.find_syntax_by_first_line(first_line))
}

/// Space-separated `hl-` classes for one scope (syntect's SpacedPrefixed).
fn scope_classes(scope: &syntect::parsing::Scope) -> String {
    let s = scope.build_string();
    let mut out = String::with_capacity(s.len() + 16);
    for (i, atom) in s.split('.').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(CLASS_PREFIX);
        out.push_str(atom);
    }
    out
}

impl Highlighter {
    /// Per-line HTML for `text` (already known to be UTF-8 and not binary).
    /// `None` when there is no grammar for the file or it is too large.
    pub fn lines(&self, id: ContentId, path: &str, text: &str) -> Option<Arc<Vec<String>>> {
        if text.len() > MAX_HIGHLIGHT_BYTES {
            return None;
        }
        let ss = syntaxes();
        let first = text.lines().next().unwrap_or("");
        let syntax = find_syntax(ss, path, first)?;
        if syntax.name == "Plain Text" {
            return None;
        }
        let key = (id, syntax_name_static(&syntax.name));
        if let Some(v) = self.cache.get(&key) {
            return Some(v);
        }
        let out = Arc::new(highlight_lines(ss, syntax, text)?);
        self.cache.insert(key, out.clone());
        Some(out)
    }

    /// The cached result, without doing any work.
    pub fn cached(&self, id: ContentId, path: &str, text: &str) -> Option<Arc<Vec<String>>> {
        let ss = SYNTAXES.get()?;
        let syntax = find_syntax(ss, path, text.lines().next().unwrap_or(""))?;
        self.cache.get(&(id, syntax_name_static(&syntax.name)))
    }

    pub fn has_grammar(&self, path: &str, text: &str) -> bool {
        text.len() <= MAX_HIGHLIGHT_BYTES
            && find_syntax(syntaxes(), path, text.lines().next().unwrap_or("")).is_some_and(|s| s.name != "Plain Text")
    }

    pub fn language(&self, path: &str, text: &str) -> Option<String> {
        let ss = syntaxes();
        find_syntax(ss, path, text.lines().next().unwrap_or("")).map(|s| s.name.clone()).filter(|n| n != "Plain Text")
    }
}

/// Intern syntax names for cache keys.
fn syntax_name_static(name: &str) -> &'static str {
    static NAMES: OnceLock<std::sync::Mutex<std::collections::HashSet<&'static str>>> = OnceLock::new();
    let set = NAMES.get_or_init(Default::default);
    let mut set = set.lock().unwrap();
    if let Some(n) = set.get(name) {
        return n;
    }
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    set.insert(leaked);
    leaked
}

fn highlight_lines(ss: &SyntaxSet, syntax: &SyntaxReference, text: &str) -> Option<Vec<String>> {
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::with_capacity(text.len() / 32 + 1);
    let style = ClassStyle::SpacedPrefixed { prefix: CLASS_PREFIX };
    for line in text.split_inclusive('\n') {
        let ops = state.parse_line(line, ss).ok()?;
        let mut html = String::with_capacity(line.len() * 3);
        // Re-open the scopes still active from previous lines.
        let reopened = stack.as_slice().len();
        for scope in stack.as_slice() {
            html.push_str("<span class=\"");
            html.push_str(&scope_classes(scope));
            html.push_str("\">");
        }
        let body = line.strip_suffix('\n').unwrap_or(line);
        let body = body.strip_suffix('\r').unwrap_or(body);
        // Ops index into the full line; clamp to the body we render.
        let ops: Vec<_> = ops.into_iter().map(|(i, op)| (i.min(body.len()), op)).collect();
        let (spans, delta) = syntect::html::line_tokens_to_classed_spans(body, &ops, style, &mut stack).ok()?;
        html.push_str(&spans);
        let open = reopened as isize + delta;
        for _ in 0..open.max(0) {
            html.push_str("</span>");
        }
        out.push(html);
    }
    if text.is_empty() {
        out.push(String::new());
    }
    Some(out)
}

/// CSS for light and dark highlight themes, scoped so the page theme
/// (`prefers-color-scheme` or an explicit `data-theme`) picks one.
pub fn theme_css() -> String {
    let themes = syntect::highlighting::ThemeSet::load_defaults();
    let style = ClassStyle::SpacedPrefixed { prefix: CLASS_PREFIX };
    let light = syntect::html::css_for_theme_with_class_style(&themes.themes["InspiredGitHub"], style).unwrap_or_default();
    let dark = syntect::html::css_for_theme_with_class_style(&themes.themes["base16-ocean.dark"], style).unwrap_or_default();
    let scope = |css: &str, prefix: &str| -> String {
        let mut out = String::with_capacity(css.len() * 2);
        for chunk in css.split_inclusive('}') {
            let Some((sel, body)) = chunk.split_once('{') else {
                out.push_str(chunk);
                continue;
            };
            // Drop the theme's page background/foreground rules: the site
            // owns those colours.
            if sel.trim().ends_with(".hl-code") {
                continue;
            }
            let scoped: Vec<String> = sel
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty() && !s.starts_with("/*"))
                .map(|s| format!("{prefix} {s}"))
                .collect();
            if scoped.is_empty() {
                continue;
            }
            out.push_str(&scoped.join(", "));
            out.push('{');
            out.push_str(body);
            out.push('\n');
        }
        out
    };
    let mut css = String::new();
    css.push_str(&scope(&strip_comments(&light), ":root:not([data-theme=dark])"));
    css.push_str("@media (prefers-color-scheme: dark) {\n");
    css.push_str(&scope(&strip_comments(&dark), ":root:not([data-theme=light])"));
    css.push_str("}\n");
    css.push_str(&scope(&strip_comments(&dark), ":root[data-theme=dark]"));
    css
}

fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        match rest[i..].find("*/") {
            Some(j) => rest = &rest[i + j + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_self_contained() {
        let h = Highlighter::default();
        let src = "fn main() {\n    /* multi\n line */ let x = \"s\";\n}\n";
        let lines = h.lines(ContentId::hash(src.as_bytes()), "a.rs", src).unwrap();
        assert_eq!(lines.len(), 4);
        for l in lines.iter() {
            assert_eq!(l.matches("<span").count(), l.matches("</span>").count(), "{l}");
        }
        assert!(lines[2].contains("hl-comment"));
        assert!(theme_css().contains("hl-keyword"));
    }
}
