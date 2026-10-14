//! Language breakdown of a tree (by bytes, from file extensions), like
//! GitHub's language bar. Computed from the cached file list.

use flux_core::RepoPath;

/// (name, color) for a path, or `None` for files that don't count
/// (docs, data, lockfiles, binaries).
pub fn language(path: &RepoPath) -> Option<(&'static str, &'static str)> {
    let name = String::from_utf8_lossy(path.basename()).to_ascii_lowercase();
    if name == "makefile" || name == "gnumakefile" {
        return Some(("Makefile", "#427819"));
    }
    if name == "dockerfile" {
        return Some(("Dockerfile", "#384d54"));
    }
    let ext = name.rsplit_once('.')?.1;
    Some(match ext {
        "rs" => ("Rust", "#dea584"),
        "c" | "h" => ("C", "#555555"),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => ("C++", "#f34b7d"),
        "go" => ("Go", "#00add8"),
        "py" => ("Python", "#3572a5"),
        "js" | "mjs" | "cjs" | "jsx" => ("JavaScript", "#f1e05a"),
        "ts" | "tsx" | "mts" => ("TypeScript", "#3178c6"),
        "java" => ("Java", "#b07219"),
        "kt" | "kts" => ("Kotlin", "#a97bff"),
        "swift" => ("Swift", "#f05138"),
        "rb" => ("Ruby", "#701516"),
        "php" => ("PHP", "#4f5d95"),
        "cs" => ("C#", "#178600"),
        "scala" => ("Scala", "#c22d40"),
        "sh" | "bash" | "zsh" => ("Shell", "#89e051"),
        "pl" | "pm" => ("Perl", "#0298c3"),
        "tcl" => ("Tcl", "#e4cc98"),
        "lua" => ("Lua", "#000080"),
        "html" | "htm" => ("HTML", "#e34c26"),
        "css" | "scss" | "sass" | "less" => ("CSS", "#663399"),
        "vue" => ("Vue", "#41b883"),
        "svelte" => ("Svelte", "#ff3e00"),
        "sql" => ("SQL", "#e38c00"),
        "proto" => ("Protocol Buffers", "#4c6aa8"),
        "ex" | "exs" => ("Elixir", "#6e4a7e"),
        "erl" => ("Erlang", "#b83998"),
        "hs" => ("Haskell", "#5e5086"),
        "ml" | "mli" => ("OCaml", "#ef7a08"),
        "clj" | "cljs" => ("Clojure", "#db5855"),
        "dart" => ("Dart", "#00b4ab"),
        "zig" => ("Zig", "#ec915c"),
        "nix" => ("Nix", "#7e7eff"),
        "m" | "mm" => ("Objective-C", "#438eff"),
        "r" => ("R", "#198ce7"),
        "jl" => ("Julia", "#a270ba"),
        "ps1" => ("PowerShell", "#012456"),
        "asm" | "s" => ("Assembly", "#6e4c13"),
        "el" => ("Emacs Lisp", "#c065db"),
        "vim" => ("Vim Script", "#199f4b"),
        "tex" => ("TeX", "#3d6117"),
        "cmake" => ("CMake", "#da3434"),
        "meson" => ("Meson", "#007800"),
        _ => return None,
    })
}

/// Languages by share of bytes, largest first; small ones fold into "Other".
pub fn breakdown(files: &[(RepoPath, flux_core::ContentId, u64)]) -> Vec<(String, String, f64)> {
    let mut by: std::collections::HashMap<&'static str, (&'static str, u64)> = std::collections::HashMap::new();
    let mut total = 0u64;
    for (p, _, size) in files {
        if let Some((name, color)) = language(p) {
            let e = by.entry(name).or_insert((color, 0));
            e.1 += size;
            total += size;
        }
    }
    if total == 0 {
        return vec![];
    }
    let mut v: Vec<(String, String, f64)> =
        by.into_iter().map(|(n, (c, b))| (n.to_string(), c.to_string(), b as f64 * 100.0 / total as f64)).collect();
    v.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    if v.len() > 7 {
        let other: f64 = v[6..].iter().map(|x| x.2).sum();
        v.truncate(6);
        v.push(("Other".into(), "#a8a193".into(), other));
    }
    v
}
