//! Branch protection and code owners.
//!
//! Rules live in the repository config (`branch_rules`). Each rule names a
//! ref pattern (`main`, `release/*`, `tags/v*`); the most specific matching
//! rule applies. Without configured rules the classic defaults hold:
//! `main`, `master`, `trunk` and `release/*` move only forward and cannot
//! be deleted, except by repository admins.
//!
//! Rules are enforced at every way a ref can move: gRPC `UpdateRef`, git
//! push (HTTP and SSH), `Land`, and landing a review. Landing a review also
//! checks the review gate: approvals, required status checks and code
//! owners.

use std::collections::{BTreeMap, BTreeSet};

use flux_core::ChangesetId;
use flux_store::RepoId;
use serde::{Deserialize, Serialize};

use crate::auth::Principal;
use crate::core::{Flux, FluxError, FluxResult};
use crate::rbac::Role;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BranchRule {
    /// Ref name or glob: `main`, `release/*`, `tags/v*`, `*`.
    pub pattern: String,
    /// Changes must land through a review (no direct pushes), except by
    /// names in `push_allow`.
    #[serde(default)]
    pub require_review: bool,
    /// Approvals needed to land a review (not counting the author).
    #[serde(default)]
    pub required_approvals: u32,
    /// Each changed file with code owners needs an approval from one of them.
    #[serde(default)]
    pub require_codeowners: bool,
    /// Approvals given before the review's last update do not count.
    #[serde(default)]
    pub dismiss_stale_approvals: bool,
    /// Status contexts that must be `success` on the review head.
    #[serde(default)]
    pub required_checks: Vec<String>,
    #[serde(default)]
    pub allow_force_push: bool,
    #[serde(default)]
    pub allow_deletion: bool,
    /// Users or `org/team`s who may push directly despite `require_review`.
    #[serde(default)]
    pub push_allow: Vec<String>,
    /// Apply the rule to repository admins too.
    #[serde(default)]
    pub enforce_admins: bool,
}

/// The classic defaults used when a repository configures no rules.
pub fn default_rules() -> Vec<BranchRule> {
    ["main", "master", "trunk", "release/*"].into_iter().map(|p| BranchRule { pattern: p.into(), ..Default::default() }).collect()
}

/// Glob match for ref names: `*` matches within a path segment, `**`
/// across segments.
pub fn ref_matches(pattern: &str, name: &str) -> bool {
    glob_match(pattern.as_bytes(), name.as_bytes())
}

fn glob_match(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') if p.get(1) == Some(&b'*') => {
            let rest = &p[2..];
            (0..=s.len()).any(|i| glob_match(rest, &s[i..]))
        }
        Some(b'*') => {
            let rest = &p[1..];
            for i in 0..=s.len() {
                if glob_match(rest, &s[i..]) {
                    return true;
                }
                if i < s.len() && s[i] == b'/' {
                    break;
                }
            }
            false
        }
        Some(b'?') => !s.is_empty() && s[0] != b'/' && glob_match(&p[1..], &s[1..]),
        Some(c) => s.first() == Some(c) && glob_match(&p[1..], &s[1..]),
    }
}

/// How specific a pattern is: exact names beat globs, longer literal
/// prefixes beat shorter ones.
fn specificity(p: &str) -> (bool, usize) {
    let literal = p.find(['*', '?']).unwrap_or(p.len());
    (!p.contains(['*', '?']), literal)
}

/// What a ref update does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefChange {
    Create,
    FastForward,
    Rewrite,
    Delete,
}

/// The state of a review against its target's protection rule.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Gate {
    pub rule: Option<String>,
    pub approvals: Vec<String>,
    pub required_approvals: u32,
    /// Status contexts and their states (`missing` if never reported).
    pub checks: Vec<(String, String)>,
    /// Files whose owners have not approved, with their owners.
    pub missing_owners: Vec<(String, Vec<String>)>,
    pub draft: bool,
    /// Why landing is blocked; empty when it may land.
    pub blockers: Vec<String>,
}

impl Gate {
    pub fn ok(&self) -> bool {
        self.blockers.is_empty()
    }
}

impl Flux {
    /// The rule protecting `name`, if any.
    pub async fn branch_rule(&self, repo: RepoId, name: &[u8]) -> FluxResult<Option<BranchRule>> {
        let cfg = self.repo_config(repo).await?;
        let rules = cfg.branch_rules.clone().unwrap_or_else(default_rules);
        let name = String::from_utf8_lossy(name);
        Ok(rules.into_iter().filter(|r| ref_matches(&r.pattern, &name)).max_by_key(|r| specificity(&r.pattern)))
    }

    async fn in_list(&self, who: &Principal, list: &[String]) -> FluxResult<bool> {
        let Some(user) = &who.user else { return Ok(false) };
        for entry in list {
            if entry == &user.name {
                return Ok(true);
            }
            if let Some((org, team)) = entry.trim_start_matches('@').split_once('/')
                && let Ok((_, t)) = self.team_named(org, team).await
                && self.meta.team_members(t.id).await?.iter().any(|m| m.id == user.id)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Check a ref move against branch protection. `via_review` is set when
    /// a review is landing (its gate is checked separately).
    pub async fn check_ref_update(
        &self,
        who: &Principal,
        repo: RepoId,
        name: &[u8],
        current: Option<ChangesetId>,
        new: Option<ChangesetId>,
        via_review: bool,
    ) -> FluxResult<()> {
        let Some(rule) = self.branch_rule(repo, name).await? else { return Ok(()) };
        let role = self.repo_role(who, repo).await?;
        if role >= Role::Admin && !rule.enforce_admins {
            return Ok(());
        }
        let change = match (current, new) {
            (None, Some(_)) => RefChange::Create,
            (Some(_), None) => RefChange::Delete,
            (Some(c), Some(n)) if c == n || self.is_ancestor(repo, &c, &n).await? => RefChange::FastForward,
            (Some(_), Some(_)) => RefChange::Rewrite,
            (None, None) => return Ok(()),
        };
        let shown = String::from_utf8_lossy(name);
        match change {
            RefChange::Delete if !rule.allow_deletion => {
                return Err(FluxError::PermissionDenied(format!("{shown} is protected: it cannot be deleted")));
            }
            RefChange::Rewrite if !rule.allow_force_push => {
                return Err(FluxError::PermissionDenied(format!(
                    "{shown} is protected: only fast-forward updates are allowed (use `flux land` to rebase onto it)"
                )));
            }
            _ => {}
        }
        if rule.require_review && !via_review && change != RefChange::Delete && !self.in_list(who, &rule.push_allow).await? {
            return Err(FluxError::PermissionDenied(format!(
                "{shown} is protected: changes must land through a review (flux review create)"
            )));
        }
        Ok(())
    }

    /// Whether a review may land, and why not.
    pub async fn review_gate(&self, repo: RepoId, number: i64) -> FluxResult<Gate> {
        let detail = self.review_detail(repo, number).await?;
        let r = &detail.review;
        let (draft, _) = self.meta.review_flags(repo, number).await?;
        let mut gate = Gate { draft, ..Default::default() };
        if r.state != "open" {
            gate.blockers.push(format!("the review is {}", r.state));
        }
        if draft {
            gate.blockers.push("the review is a draft".into());
        }
        let rule = self.branch_rule(repo, &r.target).await?;
        // Approvals: latest decision per reviewer; stale ones dropped if asked.
        let last_update = detail.events.iter().filter(|e| e.kind == "updated").map(|e| e.seq).max().unwrap_or(0);
        let mut decision: BTreeMap<&str, bool> = BTreeMap::new();
        for e in &detail.events {
            match e.kind.as_str() {
                "approved" if e.author != r.author => {
                    let stale = rule.as_ref().is_some_and(|x| x.dismiss_stale_approvals) && e.seq < last_update;
                    decision.insert(&e.author, !stale);
                }
                "changes_requested" | "dismissed" => {
                    decision.insert(&e.author, false);
                }
                _ => {}
            }
        }
        // Only approvals from people who could land it themselves count.
        for (name, ok) in &decision {
            if *ok && self.role_of_name(repo, name).await? >= Role::Write {
                gate.approvals.push(name.to_string());
            }
        }
        let changes_requested: Vec<&str> = {
            let mut last: BTreeMap<&str, &str> = BTreeMap::new();
            for e in &detail.events {
                if matches!(e.kind.as_str(), "approved" | "changes_requested" | "dismissed") {
                    last.insert(&e.author, &e.kind);
                }
            }
            last.into_iter().filter(|(_, k)| *k == "changes_requested").map(|(a, _)| a).collect()
        };
        for who in &changes_requested {
            gate.blockers.push(format!("{who} requested changes"));
        }
        let head = r.head;
        let statuses = self.meta.statuses(repo, &head).await?;
        if let Some(rule) = &rule {
            gate.rule = Some(rule.pattern.clone());
            gate.required_approvals = rule.required_approvals;
            if (gate.approvals.len() as u32) < rule.required_approvals {
                gate.blockers.push(format!("{} of {} required approvals", gate.approvals.len(), rule.required_approvals));
            }
            for ctx in &rule.required_checks {
                let state = statuses.iter().find(|s| &s.context == ctx).map_or("missing".to_string(), |s| s.state.clone());
                if state != "success" {
                    gate.blockers.push(format!("check {ctx} is {state}"));
                }
                gate.checks.push((ctx.clone(), state));
            }
            if rule.require_codeowners {
                let paths: BTreeSet<String> = detail
                    .stack
                    .iter()
                    .flat_map(|l| l.changeset.file_changes.keys().map(|p| String::from_utf8_lossy(p.as_bytes()).into_owned()))
                    .collect();
                let owners = self.codeowners(repo, detail.target_head.as_ref().unwrap_or(&head)).await?;
                let approvers: Vec<String> = gate.approvals.clone();
                for p in paths {
                    let want = owners.owners_of(&p);
                    if want.is_empty() {
                        continue;
                    }
                    let mut satisfied = false;
                    for a in &approvers {
                        if self.owner_includes(&want, a).await? {
                            satisfied = true;
                            break;
                        }
                    }
                    if !satisfied {
                        gate.missing_owners.push((p, want));
                    }
                }
                if !gate.missing_owners.is_empty() {
                    gate.blockers.push(format!("{} file(s) need a code owner's approval", gate.missing_owners.len()));
                }
            }
        }
        for s in &statuses {
            if !gate.checks.iter().any(|(c, _)| c == &s.context) {
                gate.checks.push((s.context.clone(), s.state.clone()));
            }
        }
        Ok(gate)
    }

    /// A user's role on a repository, by name (`None` role for unknown names).
    pub async fn role_of_name(&self, repo: RepoId, name: &str) -> FluxResult<Role> {
        let Some(user) = self.meta.get_user(name).await? else { return Ok(Role::None) };
        let p = Principal { user: Some(user), mode: self.auth_mode, ..Default::default() };
        self.repo_role(&p, repo).await
    }

    /// Does a CODEOWNERS owner list include `user` (directly or by team)?
    pub async fn owner_includes(&self, owners: &[String], user: &str) -> FluxResult<bool> {
        for o in owners {
            let o = o.trim_start_matches('@');
            if o == user {
                return Ok(true);
            }
            if let Some((org, team)) = o.split_once('/')
                && let Ok((_, t)) = self.team_named(org, team).await
                && self.meta.team_members(t.id).await?.iter().any(|m| m.name == user)
            {
                return Ok(true);
            }
            if o.contains('@')
                && let Some((a, _)) = self.meta.get_account(o).await?
                && a.user.name == user
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The CODEOWNERS file at a changeset (first of
    /// `CODEOWNERS`, `.github/CODEOWNERS`, `docs/CODEOWNERS`).
    pub async fn codeowners(&self, repo: RepoId, at: &ChangesetId) -> FluxResult<CodeOwners> {
        for p in ["CODEOWNERS", ".github/CODEOWNERS", "docs/CODEOWNERS"] {
            let path = flux_core::RepoPath::new(p.as_bytes().to_vec())?;
            if let Some(e) = self.entry_at(repo, at, &path).await?
                && !e.is_dir()
            {
                let id = flux_core::ContentId::from_bytes(e.id);
                if let Some(data) = self.blobs.get_content_bytes(&id).await? {
                    return Ok(CodeOwners::parse(&String::from_utf8_lossy(&data)));
                }
            }
        }
        Ok(CodeOwners::default())
    }
}

/// A parsed CODEOWNERS file. The last matching rule wins.
#[derive(Debug, Clone, Default)]
pub struct CodeOwners {
    rules: Vec<(regex::Regex, Vec<String>)>,
}

impl CodeOwners {
    pub fn parse(text: &str) -> Self {
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.split(" #").next().unwrap_or("").trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(pattern) = parts.next() else { continue };
            let owners: Vec<String> = parts.map(str::to_string).collect();
            if let Some(re) = codeowners_regex(pattern) {
                rules.push((re, owners));
            }
        }
        Self { rules }
    }

    /// Owners of a path (empty when unowned).
    pub fn owners_of(&self, path: &str) -> Vec<String> {
        self.rules.iter().rev().find(|(re, _)| re.is_match(path)).map(|(_, o)| o.clone()).unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// gitignore-style pattern → regex over repository paths.
fn codeowners_regex(pattern: &str) -> Option<regex::Regex> {
    let anchored = pattern.starts_with('/') || pattern.trim_end_matches('/').contains('/');
    let dir = pattern.ends_with('/');
    let p = pattern.trim_start_matches('/').trim_end_matches('/');
    if p.is_empty() {
        return None;
    }
    let mut re = String::from(if anchored { "^" } else { "^(?:.*/)?" });
    let b = p.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'*' if b.get(i + 1) == Some(&b'*') => {
                if b.get(i + 2) == Some(&b'/') {
                    re.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    re.push_str(".*");
                    i += 2;
                }
                continue;
            }
            b'*' => re.push_str("[^/]*"),
            b'?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&(c as char).to_string())),
        }
        i += 1;
    }
    // A pattern matches the path itself or anything under it (directories).
    re.push_str(if dir { "/.*$" } else { "(?:/.*)?$" });
    regex::Regex::new(&re).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(ref_matches("main", "main"));
        assert!(!ref_matches("main", "main2"));
        assert!(ref_matches("release/*", "release/1.0"));
        assert!(!ref_matches("release/*", "release/1.0/hotfix"));
        assert!(ref_matches("release/**", "release/1.0/hotfix"));
        assert!(ref_matches("tags/v*", "tags/v1.2"));
        assert!(ref_matches("*", "feature"));
        assert!(!ref_matches("*", "feature/x"));
        assert!(specificity("main") > specificity("ma*"));
        assert!(specificity("release/1*") > specificity("release/*"));
    }

    #[test]
    fn codeowners_last_match_wins() {
        let co = CodeOwners::parse(
            "# comment\n* @everyone\n*.rs @rustaceans\n/docs/ @writers\nsrc/ui/** @acme/frontend\n/infra/secrets @ops # trailing\n",
        );
        assert_eq!(co.owners_of("README.md"), vec!["@everyone"]);
        assert_eq!(co.owners_of("crates/x/src/lib.rs"), vec!["@rustaceans"]);
        assert_eq!(co.owners_of("docs/guide/intro.md"), vec!["@writers"]);
        assert_eq!(co.owners_of("src/ui/button.tsx"), vec!["@acme/frontend"]);
        assert_eq!(co.owners_of("src/ui/deep/x.rs"), vec!["@acme/frontend"]);
        assert_eq!(co.owners_of("infra/secrets/key.pem"), vec!["@ops"]);
        assert_eq!(co.owners_of("other/infra/secrets"), vec!["@everyone"]);
        assert!(CodeOwners::parse("").owners_of("x").is_empty());
    }
}

/// How a review's stack lands on its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeMethod {
    /// Replay each changeset onto the target (fast-forward when possible).
    #[default]
    Rebase,
    /// One changeset with the stack's combined changes.
    Squash,
}

impl std::str::FromStr for MergeMethod {
    type Err = FluxError;
    fn from_str(s: &str) -> FluxResult<Self> {
        match s {
            "" | "rebase" => Ok(Self::Rebase),
            "squash" => Ok(Self::Squash),
            o => Err(FluxError::InvalidArgument(format!("unknown merge method {o:?} (rebase | squash)"))),
        }
    }
}

/// Policy-checked entry points shared by gRPC, git, the web UI and the API:
/// role, archived state, path rules and branch protection.
impl Flux {
    pub fn check_ref_limits(&self, who: &Principal, name: &[u8]) -> FluxResult<()> {
        if let Some(pats) = crate::rbac::ref_limits(&who.scopes) {
            let n = String::from_utf8_lossy(name);
            if !pats.iter().any(|p| ref_matches(p, &n)) {
                return Err(FluxError::PermissionDenied(format!("this token may only move refs matching {}", pats.join(", "))));
            }
        }
        Ok(())
    }

    pub async fn check_writable(&self, repo: RepoId) -> FluxResult<()> {
        if self.repo_config(repo).await?.archived {
            return Err(FluxError::FailedPrecondition("this repository is archived (read-only)".into()));
        }
        Ok(())
    }

    /// Move a ref on behalf of `who`.
    pub async fn update_ref_as(
        &self,
        who: &Principal,
        repo: RepoId,
        name: &[u8],
        expect: flux_store::RefExpect,
        new: Option<ChangesetId>,
        reason: String,
    ) -> FluxResult<(flux_store::CasOutcome, Option<flux_store::RefValue>)> {
        use flux_store::RefExpect;
        self.authorize(who, repo, Role::Write).await?;
        self.check_writable(repo).await?;
        crate::core::validate_ref_name(name)?;
        self.check_ref_limits(who, name)?;
        let current = self.meta.get_ref(repo, name).await?;
        self.check_ref_update(who, repo, name, current.map(|c| c.target), new, false).await?;
        if let Some(new) = new
            && let Some(cur) = current
            && cur.target != new
        {
            let paths = self.changed_paths(repo, &cur.target, &new).await?;
            self.check_write_paths(repo, who, paths).await?;
        }
        // Pin an unconditional update to the value the checks saw.
        let expect = match (expect, current) {
            (RefExpect::Any, Some(cur)) if self.branch_rule(repo, name).await?.is_some() => RefExpect::Version(cur.version),
            (e, _) => e,
        };
        self.update_ref(repo, name, expect, new, who.name(), reason).await
    }

    /// Paths changed between two changesets (for path rules).
    async fn changed_paths(&self, repo: RepoId, old: &ChangesetId, new: &ChangesetId) -> FluxResult<Vec<Vec<u8>>> {
        if !self.repo_config(repo).await?.has_rules() {
            return Ok(vec![]);
        }
        let (a, b) = (self.root_tree(repo, old).await?, self.root_tree(repo, new).await?);
        let diff = flux_derive::diff_trees(&self.blobs, Some(a), Some(b), &|_, _| true).await?;
        Ok(diff.into_iter().map(|d| d.path.as_bytes().to_vec()).collect())
    }

    /// Land a stack directly (not through a review).
    pub async fn land_as(
        self: &std::sync::Arc<Self>,
        who: &Principal,
        repo: RepoId,
        top: ChangesetId,
        target: &[u8],
        reason: &str,
    ) -> FluxResult<crate::land::LandResult> {
        self.authorize(who, repo, Role::Write).await?;
        self.check_writable(repo).await?;
        self.check_ref_limits(who, target)?;
        self.check_land_paths(repo, who, &top, target).await?;
        if let Some(rule) = self.branch_rule(repo, target).await?
            && rule.require_review
            && (self.repo_role(who, repo).await? < Role::Admin || rule.enforce_admins)
            && !self.in_list(who, &rule.push_allow).await?
        {
            return Err(FluxError::PermissionDenied(format!(
                "{} is protected: changes must land through a review",
                String::from_utf8_lossy(target)
            )));
        }
        self.land(repo, top, target, who.name(), reason).await
    }

    /// Land a review after checking its gate. Repository admins may land
    /// past a failing gate unless the rule enforces admins.
    pub async fn land_review_as(
        self: &std::sync::Arc<Self>,
        who: &Principal,
        repo: RepoId,
        number: i64,
        method: MergeMethod,
    ) -> FluxResult<crate::land::LandResult> {
        self.authorize(who, repo, Role::Write).await?;
        self.check_writable(repo).await?;
        let rv = self.review(repo, number).await?;
        self.check_ref_limits(who, &rv.target)?;
        self.check_land_paths(repo, who, &rv.head, &rv.target).await?;
        let gate = self.review_gate(repo, number).await?;
        if !gate.ok() {
            let rule = self.branch_rule(repo, &rv.target).await?;
            let admin_bypass =
                self.repo_role(who, repo).await? >= Role::Admin && !rule.as_ref().is_some_and(|r| r.enforce_admins);
            // Closed or draft reviews never land.
            if !admin_bypass || gate.draft || rv.state != "open" {
                return Err(FluxError::FailedPrecondition(format!("cannot land #{number}: {}", gate.blockers.join("; "))));
            }
        }
        self.land_review(repo, number, who.name(), method).await
    }
}
