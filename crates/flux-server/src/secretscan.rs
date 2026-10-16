//! Push protection: refuse new content that contains credentials.
//!
//! Every way new file content enters a repository (git push over HTTP or
//! SSH, the REST `changes` endpoint, web edits) passes its added text
//! through these detectors. High-confidence patterns only (provider token
//! formats and private key headers), so false positives stay rare. A
//! repository turns it off with `secret_scanning: false` in its settings;
//! a line containing `flux:allow-secret` is skipped (test fixtures).

use std::sync::LazyLock;

use regex::bytes::Regex;

struct Detector {
    name: &'static str,
    re: Regex,
}

static DETECTORS: LazyLock<Vec<Detector>> = LazyLock::new(|| {
    [
        ("AWS access key ID", r"\b(?:AKIA|ASIA|AGPA|AIDA|AROA|ANPA|ANVA|AIPA)[0-9A-Z]{16}\b"),
        ("GitHub token", r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,255}\b"),
        ("GitHub fine-grained token", r"\bgithub_pat_[A-Za-z0-9_]{82}\b"),
        ("GitLab token", r"\bglpat-[A-Za-z0-9_\-]{20}\b"),
        ("flux token", r"\bflx[r]?_[0-9a-f]{64}\b"),
        ("Slack token", r"\bxox[abposr]-[0-9A-Za-z\-]{10,72}\b"),
        ("Slack webhook", r"https://hooks\.slack\.com/services/T[A-Z0-9]{8,}/B[A-Z0-9]{8,}/[A-Za-z0-9]{24}"),
        ("Stripe secret key", r"\b(?:sk|rk)_live_[0-9A-Za-z]{24,99}\b"),
        ("Google API key", r"\bAIza[0-9A-Za-z_\-]{35}\b"),
        ("OpenAI API key", r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_\-]{20,}T3BlbkFJ[A-Za-z0-9_\-]{20,}\b"),
        ("Anthropic API key", r"\bsk-ant-(?:api|admin)\d{2}-[A-Za-z0-9_\-]{80,120}\b"),
        ("npm token", r"\bnpm_[A-Za-z0-9]{36}\b"),
        ("PyPI token", r"\bpypi-AgEIcHlwaS5vcmc[A-Za-z0-9_\-]{50,}\b"),
        ("private key", r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |PGP |ENCRYPTED )?PRIVATE KEY(?: BLOCK)?-----"),
    ]
    .into_iter()
    .map(|(name, pat)| Detector { name, re: Regex::new(pat).expect("valid detector") })
    .collect()
});

/// Marks a line as a deliberate fixture.
const ALLOW: &[u8] = b"flux:allow-secret";

/// A credential found in new content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub path: String,
    pub line: usize,
    pub kind: &'static str,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.path, self.line, self.kind)
    }
}

/// Scan one file's content. Binary files and very large files are skipped.
pub fn scan(path: &str, data: &[u8]) -> Vec<Finding> {
    if data.len() > 8 << 20 || data[..data.len().min(8192)].contains(&0) {
        return vec![];
    }
    let mut out = Vec::new();
    for (i, line) in data.split(|b| *b == b'\n').enumerate() {
        if line.len() > 16 * 1024 || line.windows(ALLOW.len()).any(|w| w == ALLOW) {
            continue;
        }
        for d in DETECTORS.iter() {
            if d.re.is_match(line) {
                out.push(Finding { path: path.to_string(), line: i + 1, kind: d.name });
                if out.len() >= 20 {
                    return out;
                }
            }
        }
    }
    out
}

/// The error for a refused write.
pub fn refusal(findings: &[Finding]) -> crate::core::FluxError {
    let list: Vec<String> = findings.iter().take(10).map(ToString::to_string).collect();
    crate::core::FluxError::PermissionDenied(format!(
        "push protection: this change contains what looks like a secret ({}). Remove it (and rotate it), \
         add `flux:allow-secret` to the line if it is a test fixture, or turn push protection off in the \
         repository settings",
        list.join("; ")
    ))
}

impl crate::core::Flux {
    /// Scan new contents for a repository (no-op when scanning is off).
    pub async fn scan_new_contents(
        &self,
        repo: flux_store::RepoId,
        files: &[(String, bytes::Bytes)],
    ) -> crate::core::FluxResult<()> {
        if self.repo_config(repo).await?.secret_scanning_disabled {
            return Ok(());
        }
        let findings: Vec<Finding> = files.iter().flat_map(|(p, d)| scan(p, d)).collect();
        if findings.is_empty() { Ok(()) } else { Err(refusal(&findings)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_credentials() {
        // Assembled at runtime so this file does not trip scanners itself.
        let aws = format!("key = \"{}{}\"\n", "AKIA", "IOSFODNN7EXAMPLE");
        let gh = format!("token: {}_{}\n", "ghp", "a".repeat(36));
        let pk = format!("-----BEGIN {} PRIVATE KEY-----\n", "OPENSSH");
        let text = format!("fine\n{aws}{gh}{pk}");
        let f = scan("config.yml", text.as_bytes());
        assert_eq!(
            f.iter().map(|f| (f.line, f.kind)).collect::<Vec<_>>(),
            vec![(2, "AWS access key ID"), (3, "GitHub token"), (4, "private key")]
        );
        let allowed = format!("{} # flux:allow-secret\n", aws.trim());
        assert!(scan("t.rs", allowed.as_bytes()).is_empty());
        assert!(scan("img.png", b"\0\0AKIAIOSFODNN7EXAMPLE").is_empty());
        assert!(scan("a.txt", b"AKIA is a prefix; ghp_short").is_empty());
    }
}
