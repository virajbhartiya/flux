//! What a migration did: counts per kind, warnings, and time per phase.

use std::collections::BTreeMap;
use std::time::Instant;

use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Phase {
    pub name: String,
    pub ms: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub github: String,
    pub flux_repo: String,
    /// The repository's web page on flux.
    pub flux_url: String,
    /// The git remote to switch to.
    pub clone_url: String,
    pub created: bool,
    /// Whether this was an incremental re-sync.
    pub incremental: bool,
    pub counts: BTreeMap<String, u64>,
    pub warnings: Vec<String>,
    /// Actions secrets on GitHub, to re-enter on flux.
    pub secrets: Vec<String>,
    pub phases: Vec<Phase>,
    pub github_requests: u64,
    pub elapsed_ms: u64,
}

impl Report {
    pub fn add(&mut self, kind: &str, n: u64) {
        if n > 0 {
            *self.counts.entry(kind.to_string()).or_default() += n;
        }
    }

    pub fn count(&self, kind: &str) -> u64 {
        self.counts.get(kind).copied().unwrap_or(0)
    }

    pub fn warn(&mut self, w: impl Into<String>) {
        self.warnings.push(w.into());
    }

    pub fn phase(&mut self, name: &str, started: Instant) {
        self.phases.push(Phase { name: name.into(), ms: started.elapsed().as_millis() as u64 });
    }

    /// `12 commits · 3 issues · …` (kinds with a count only).
    pub fn summary(&self) -> String {
        if self.counts.is_empty() {
            return "no changes".into();
        }
        self.counts.iter().map(|(k, v)| format!("{v} {}", k.replace('_', " "))).collect::<Vec<_>>().join(" · ")
    }
}
