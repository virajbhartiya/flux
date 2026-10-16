//! `uses:` handlers. flux-runner does not run JavaScript actions; it has
//! native implementations for a few and skips (with a note) the ones whose
//! effect is not needed on a self-hosted machine.

/// How a `uses:` step is handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handler {
    /// Native `actions/checkout`.
    Checkout,
    /// `docker://image`: run the image with the workspace mounted.
    Docker(String),
    /// Known action, deliberately skipped; the note says why.
    Skip(&'static str),
    Unsupported,
}

/// Action name prefixes (lower case, without `@ref`) and their handling.
const TABLE: &[(&str, Option<&str>)] = &[
    ("actions/checkout", None),
    ("actions/setup-", Some("set-up actions are not run; the runner's installed toolchains are used")),
    ("actions/cache", Some("caching is not available in flux-runner; steps run without a cache")),
    ("actions/upload-artifact", Some("artifacts are not stored by flux-runner")),
    ("actions/download-artifact", Some("artifacts are not stored by flux-runner")),
];

pub fn handler(uses: &str) -> Handler {
    let uses = uses.trim();
    if let Some(image) = uses.strip_prefix("docker://") {
        return Handler::Docker(image.to_string());
    }
    let name = uses.split('@').next().unwrap_or(uses).to_ascii_lowercase();
    for (prefix, skip) in TABLE {
        let hit = if prefix.ends_with('-') {
            name.starts_with(prefix)
        } else {
            name == *prefix || name.starts_with(&format!("{prefix}/"))
        };
        if hit {
            return match skip {
                None => Handler::Checkout,
                Some(note) => Handler::Skip(note),
            };
        }
    }
    Handler::Unsupported
}

/// Is `s` a full hexadecimal commit id?
pub fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// For a checkout `ref`, the remote refs to try and the local ref each is
/// fetched into.
pub fn ref_candidates(r: &str) -> Vec<(String, String)> {
    if let Some(b) = r.strip_prefix("refs/heads/") {
        return vec![(r.to_string(), format!("refs/remotes/origin/{b}"))];
    }
    if r.starts_with("refs/") {
        return vec![(r.to_string(), r.to_string())];
    }
    vec![(format!("refs/heads/{r}"), format!("refs/remotes/origin/{r}")), (format!("refs/tags/{r}"), format!("refs/tags/{r}"))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handlers() {
        assert_eq!(handler("actions/checkout@v4"), Handler::Checkout);
        assert_eq!(handler("Actions/Checkout@main"), Handler::Checkout);
        assert_eq!(handler("actions/checkout-extra@v1"), Handler::Unsupported);
        assert_eq!(handler("docker://alpine:3.20"), Handler::Docker("alpine:3.20".into()));
        assert!(matches!(handler("actions/setup-node@v4"), Handler::Skip(_)));
        assert!(matches!(handler("actions/cache@v4"), Handler::Skip(_)));
        assert!(matches!(handler("actions/cache/restore@v4"), Handler::Skip(_)));
        assert!(matches!(handler("actions/upload-artifact@v4"), Handler::Skip(_)));
        assert_eq!(handler("dtolnay/rust-toolchain@stable"), Handler::Unsupported);
        assert_eq!(handler("./.github/actions/local"), Handler::Unsupported);
    }

    #[test]
    fn refs() {
        assert!(is_sha("0123456789abcdef0123456789ABCDEF01234567"));
        assert!(!is_sha("main"));
        assert_eq!(ref_candidates("refs/heads/main"), vec![("refs/heads/main".into(), "refs/remotes/origin/main".into())]);
        assert_eq!(ref_candidates("v1")[1], ("refs/tags/v1".into(), "refs/tags/v1".into()));
    }
}
