//! What a migration cannot carry over: webhooks and secrets (their secrets
//! are unreadable), Actions variables, rulesets, Pages and Discussions,
//! and workflow features flux CI does not run.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use serde_yaml::Value;

use crate::github::{GitHub, status_of};
use crate::model::{Hook, Named, Repo};
use crate::report::Report;

const TRIGGERS: [&str; 3] = ["push", "pull_request", "workflow_dispatch"];

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Sequence(s)) => s.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        Some(Value::Mapping(m)) => m.keys().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => vec![],
    }
}

fn list(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(", ")
}

/// Warnings for one workflow file.
pub fn workflow_warnings(path: &str, yaml: &[u8]) -> Vec<String> {
    let doc: Value = match serde_yaml::from_slice(yaml) {
        Ok(v) => v,
        Err(e) => return vec![format!("{path}: not valid YAML ({e})")],
    };
    let mut out = Vec::new();
    let triggers: BTreeSet<String> = strings(doc.get("on")).into_iter().filter(|t| !TRIGGERS.contains(&t.as_str())).collect();
    if !triggers.is_empty() {
        out.push(format!(
            "{path}: triggers {} do not run on flux CI (it runs push, pull_request and workflow_dispatch)",
            list(&triggers)
        ));
    }
    let (mut actions, mut services, mut reusable) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    for (id, job) in doc.get("jobs").and_then(Value::as_mapping).into_iter().flatten() {
        let id = id.as_str().unwrap_or_default().to_string();
        if job.get("uses").is_some() {
            reusable.insert(id.clone());
        }
        if job.get("services").is_some() {
            services.insert(id.clone());
        }
        for step in job.get("steps").and_then(Value::as_sequence).into_iter().flatten() {
            if let Some(uses) = step.get("uses").and_then(Value::as_str) {
                let name = uses.split('@').next().unwrap_or(uses);
                if name != "actions/checkout" {
                    actions.insert(name.to_string());
                }
            }
        }
    }
    if !actions.is_empty() {
        out.push(format!("{path}: actions {} run only on runners with a handler for them", list(&actions)));
    }
    if !services.is_empty() {
        out.push(format!("{path}: service containers (jobs {}) are not supported", list(&services)));
    }
    if !reusable.is_empty() {
        out.push(format!("{path}: reusable workflow calls (jobs {}) are not supported", list(&reusable)));
    }
    out
}

/// Check everything that needs a person after the migration.
pub async fn audit(gh: &GitHub, repo: &Repo, flux_repo: &str, git_dir: &Path, report: &mut Report) -> Result<()> {
    let base = format!("/repos/{}", repo.full_name);
    match gh.list::<Hook>(&format!("{base}/hooks")).await {
        Ok(hooks) => {
            for h in hooks {
                let url = h.config.url.unwrap_or_else(|| format!("hook {}", h.id));
                report.warn(format!(
                    "webhook {url} ({}{}): recreate it with POST /api/v1/repos/{flux_repo}/hooks; flux sends GitHub-compatible payloads signed with X-Hub-Signature-256, but the secret cannot be read from GitHub",
                    h.events.join(", "),
                    if h.active { "" } else { ", inactive" }
                ));
            }
        }
        Err(e) if matches!(status_of(&e), Some(403 | 404)) => {
            report.warn("could not list webhooks (needs admin access to the GitHub repository); recreate them by hand")
        }
        Err(e) => return Err(e),
    }
    match gh.get::<Named>(&format!("{base}/actions/secrets?per_page=100")).await {
        Ok(n) if !n.secrets.is_empty() => {
            report.secrets = n.secrets.into_iter().map(|s| s.name).collect();
            report.warn(format!(
                "Actions secrets must be re-entered (values cannot be read from GitHub): {} — PUT /api/v1/repos/{flux_repo}/secrets/<name>",
                report.secrets.join(", ")
            ));
        }
        Ok(_) => {}
        Err(e) if matches!(status_of(&e), Some(403 | 404)) => {
            report.warn("could not list Actions secrets (needs admin access); re-enter the secrets your workflows use")
        }
        Err(e) => return Err(e),
    }
    if let Ok(n) = gh.get::<Named>(&format!("{base}/actions/variables?per_page=100")).await
        && !n.variables.is_empty()
    {
        let names: Vec<String> = n.variables.into_iter().map(|v| v.name).collect();
        report.warn(format!("Actions variables are not migrated: {}", names.join(", ")));
    }
    if let Ok(rules) = gh.get::<Vec<serde_json::Value>>(&format!("{base}/rulesets")).await
        && !rules.is_empty()
    {
        report.warn("repository rulesets are not migrated (classic branch protection of the default branch is)");
    }
    if repo.has_pages {
        report.warn("GitHub Pages is not migrated");
    }
    if repo.has_discussions {
        report.warn("Discussions are not migrated");
    }
    for (path, data) in crate::gitcache::workflows(git_dir, &repo.default_branch).await? {
        for w in workflow_warnings(&path, &data) {
            report.warn(w);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_features() {
        let yaml = br#"
name: CI
on:
  push:
  pull_request_target:
  schedule:
    - cron: "0 0 * * *"
jobs:
  test:
    runs-on: ubuntu-latest
    services:
      db: { image: postgres }
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
      - run: npm test
  shared:
    uses: acme/workflows/.github/workflows/ci.yml@main
"#;
        let w = workflow_warnings("ci.yml", yaml);
        assert_eq!(w.len(), 4, "{w:#?}");
        assert!(w[0].contains("pull_request_target, schedule"));
        assert!(w[1].contains("actions/setup-node"));
        assert!(w[2].contains("test"));
        assert!(w[3].contains("shared"));
        assert!(
            workflow_warnings("ok.yml", b"on: [push, pull_request]\njobs:\n  a:\n    steps:\n      - run: make\n").is_empty()
        );
        assert_eq!(workflow_warnings("bad.yml", b"on: [").len(), 1);
    }
}
