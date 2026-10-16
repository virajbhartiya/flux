//! Commit statuses: what CI systems report about a changeset.
//!
//! External CI posts `pending`, `success`, `failure` or `error` per
//! context; the combined state feeds branch protection, the web UI and
//! agents waiting on results (`wait_for_checks`).

use flux_core::ChangesetId;
use flux_store::{CommitStatus, RepoId};
use serde::Serialize;

use crate::auth::Principal;
use crate::core::{Event, Flux, FluxError, FluxResult};
use crate::rbac::Role;

pub const STATES: [&str; 4] = ["pending", "success", "failure", "error"];

#[derive(Debug, Clone, Serialize)]
pub struct StatusJson {
    pub context: String,
    pub state: String,
    pub description: String,
    pub target_url: String,
    pub creator: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<&CommitStatus> for StatusJson {
    fn from(s: &CommitStatus) -> Self {
        Self {
            context: s.context.clone(),
            state: s.state.clone(),
            description: s.description.clone(),
            target_url: s.target_url.clone(),
            creator: s.creator.clone(),
            created_at: s.created_ms,
            updated_at: s.updated_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Combined {
    /// `success` when every context succeeded, `failure` when any failed or
    /// errored, otherwise `pending` (also when nothing reported yet).
    pub state: String,
    pub total: usize,
    pub statuses: Vec<StatusJson>,
}

pub fn combine(statuses: &[CommitStatus]) -> String {
    if statuses.iter().any(|s| s.state == "failure" || s.state == "error") {
        "failure".into()
    } else if !statuses.is_empty() && statuses.iter().all(|s| s.state == "success") {
        "success".into()
    } else {
        "pending".into()
    }
}

impl Flux {
    pub async fn post_status(
        &self,
        who: &Principal,
        repo: RepoId,
        cs: ChangesetId,
        state: &str,
        context: &str,
        description: &str,
        target_url: &str,
    ) -> FluxResult<()> {
        self.authorize(who, repo, Role::Write).await?;
        self.set_status(repo, cs, state, context, description, target_url, who.name()).await
    }

    /// Record a status without a permission check (CI and internal callers).
    pub async fn set_status(
        &self,
        repo: RepoId,
        cs: ChangesetId,
        state: &str,
        context: &str,
        description: &str,
        target_url: &str,
        creator: &str,
    ) -> FluxResult<()> {
        if !STATES.contains(&state) {
            return Err(FluxError::InvalidArgument(format!("state must be one of {}", STATES.join(", "))));
        }
        let context = if context.trim().is_empty() { "default" } else { context.trim() };
        if context.len() > 255 || description.len() > 1024 || target_url.len() > 2048 {
            return Err(FluxError::InvalidArgument("context, description or URL too long".into()));
        }
        if !target_url.is_empty() && !(target_url.starts_with("https://") || target_url.starts_with("http://")) {
            return Err(FluxError::InvalidArgument("target_url must be an http(s) URL".into()));
        }
        self.load_one_arc(repo, &cs).await?;
        let s = CommitStatus {
            context: context.into(),
            state: state.into(),
            description: description.into(),
            target_url: target_url.into(),
            creator: creator.into(),
            created_ms: 0,
            updated_ms: 0,
        };
        self.meta.put_status(repo, &cs, &s).await?;
        let name = self.repo_name(repo).await;
        self.publish(Event::Status { repo: name, cs, context: context.into(), state: state.into(), actor: creator.into() });
        Ok(())
    }

    pub async fn combined_status(&self, repo: RepoId, cs: &ChangesetId) -> FluxResult<Combined> {
        let statuses = self.meta.statuses(repo, cs).await?;
        Ok(Combined {
            state: combine(&statuses),
            total: statuses.len(),
            statuses: statuses.iter().map(StatusJson::from).collect(),
        })
    }

    /// Wait (up to `timeout`) until no reported check is pending and at
    /// least `min_contexts` reported. Returns the combined status then, or
    /// at the timeout.
    pub async fn wait_for_checks(
        &self,
        repo: RepoId,
        cs: &ChangesetId,
        min_contexts: usize,
        timeout: std::time::Duration,
    ) -> FluxResult<Combined> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = self.subscribe();
        let name = self.repo_name(repo).await;
        loop {
            let c = self.combined_status(repo, cs).await?;
            let settled = c.total >= min_contexts.max(1) && c.statuses.iter().all(|s| s.state != "pending");
            if settled || tokio::time::Instant::now() >= deadline {
                return Ok(c);
            }
            // Wake on any status for this changeset (or re-poll every 5 s).
            let wait =
                tokio::time::timeout_at(deadline.min(tokio::time::Instant::now() + std::time::Duration::from_secs(5)), async {
                    loop {
                        match rx.recv().await {
                            Ok(Event::Status { repo, cs: c, .. }) if repo == name && &c == cs => return,
                            Ok(_) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => return,
                            Err(_) => return,
                        }
                    }
                });
            let _ = wait.await;
        }
    }
}
