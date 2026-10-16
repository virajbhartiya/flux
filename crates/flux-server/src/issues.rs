//! Issues, comments, labels, assignees and milestones.
//!
//! Issues share the number space with reviews, and labels, assignees and
//! milestones apply to both. Permissions follow GitHub's model: anyone who
//! can read a repository may open issues and comment; triage may label,
//! assign, close and lock; write may manage labels and milestones. Authors
//! may edit, close and reopen their own issues.

use flux_store::{Issue, IssueEvent, IssueQuery, IssueUpdate, Label, Milestone, NewIssue, RepoId};
use serde::{Deserialize, Serialize};

use crate::auth::Principal;
use crate::core::{Event, Flux, FluxError, FluxResult};
use crate::rbac::Role;

const MAX_TITLE: usize = 256;
const MAX_BODY: usize = 65_536;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct IssueInput {
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignees: Vec<String>,
    #[serde(default)]
    pub milestone: Option<i64>,
}

/// Fields to change; `None` leaves a field alone.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct IssueEdit {
    pub title: Option<String>,
    pub body: Option<String>,
    /// `open` or `closed`.
    pub state: Option<String>,
    pub state_reason: Option<String>,
    /// `Some(None)` clears the milestone.
    #[serde(default, deserialize_with = "double_option")]
    pub milestone: Option<Option<i64>>,
    pub labels: Option<Vec<String>>,
    pub assignees: Option<Vec<String>>,
    pub locked: Option<bool>,
}

fn double_option<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<i64>>, D::Error> {
    Ok(Some(Option::deserialize(d)?))
}

/// An issue with everything shown on its page.
#[derive(Debug, Clone, Serialize)]
pub struct IssueView {
    #[serde(flatten)]
    pub issue: IssueJson,
    pub timeline: Vec<EventJson>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueJson {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub author: String,
    pub state: String,
    pub state_reason: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub milestone: Option<i64>,
    pub locked: bool,
    pub comments: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub closed_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventJson {
    pub id: i64,
    pub kind: String,
    pub author: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<&IssueEvent> for EventJson {
    fn from(e: &IssueEvent) -> Self {
        Self {
            id: e.seq,
            kind: e.kind.clone(),
            author: e.author.clone(),
            body: e.body.clone(),
            created_at: e.created_ms,
            updated_at: e.updated_ms,
        }
    }
}

pub fn valid_color(c: &str) -> bool {
    c.len() == 6 && c.bytes().all(|b| b.is_ascii_hexdigit())
}

fn check_text(title: &str, body: &str) -> FluxResult<()> {
    if title.trim().is_empty() {
        return Err(FluxError::InvalidArgument("a title is required".into()));
    }
    if title.len() > MAX_TITLE {
        return Err(FluxError::InvalidArgument(format!("titles are limited to {MAX_TITLE} bytes")));
    }
    if body.len() > MAX_BODY {
        return Err(FluxError::InvalidArgument(format!("text is limited to {MAX_BODY} bytes")));
    }
    Ok(())
}

impl Flux {
    async fn issues_on(&self, repo: RepoId) -> FluxResult<()> {
        if self.repo_config(repo).await?.issues_disabled {
            return Err(FluxError::NotFound("issues are turned off for this repository".into()));
        }
        Ok(())
    }

    async fn issue_or_404(&self, repo: RepoId, number: i64) -> FluxResult<Issue> {
        self.meta.get_issue(repo, number).await?.ok_or_else(|| FluxError::NotFound(format!("issue #{number} not found")))
    }

    async fn publish_issue(&self, repo: RepoId, number: i64, action: &str, actor: &str) {
        let name = self.repo_name(repo).await;
        self.publish(Event::Issue { repo: name, number, action: action.into(), actor: actor.into() });
    }

    /// The JSON shape of issues, with labels and assignees loaded in bulk.
    pub async fn issues_json(&self, repo: RepoId, issues: &[Issue]) -> FluxResult<Vec<IssueJson>> {
        let numbers: Vec<i64> = issues.iter().map(|i| i.number).collect();
        let labels = self.meta.item_labels(repo, &numbers).await?;
        let people = self.meta.item_people(repo, &numbers, "assignee").await?;
        Ok(issues
            .iter()
            .map(|i| IssueJson {
                number: i.number,
                title: i.title.clone(),
                body: i.body.clone(),
                author: i.author.clone(),
                state: i.state.clone(),
                state_reason: i.state_reason.clone(),
                labels: labels.iter().filter(|(n, _)| *n == i.number).map(|(_, l)| l.clone()).collect(),
                assignees: people.iter().filter(|(n, _)| *n == i.number).map(|(_, p)| p.clone()).collect(),
                milestone: i.milestone,
                locked: i.locked,
                comments: i.comments,
                created_at: i.created_ms,
                updated_at: i.updated_ms,
                closed_at: i.closed_ms,
            })
            .collect())
    }

    pub async fn list_issues_as(&self, who: &Principal, repo: RepoId, q: &IssueQuery) -> FluxResult<(Vec<IssueJson>, i64)> {
        self.authorize(who, repo, Role::Read).await?;
        self.issues_on(repo).await?;
        let (items, total) = self.meta.list_issues(repo, q).await?;
        Ok((self.issues_json(repo, &items).await?, total))
    }

    pub async fn issue_view(&self, who: &Principal, repo: RepoId, number: i64) -> FluxResult<IssueView> {
        self.authorize(who, repo, Role::Read).await?;
        self.issues_on(repo).await?;
        let issue = self.issue_or_404(repo, number).await?;
        let events = self.meta.issue_events(repo, number).await?;
        let issue = self.issues_json(repo, std::slice::from_ref(&issue)).await?.remove(0);
        Ok(IssueView { issue, timeline: events.iter().map(EventJson::from).collect() })
    }

    pub async fn open_issue(&self, who: &Principal, repo: RepoId, input: IssueInput) -> FluxResult<IssueJson> {
        let role = self.authorize(who, repo, Role::Read).await?;
        self.issues_on(repo).await?;
        let user = who.user.as_ref().ok_or_else(|| FluxError::Unauthenticated("sign in to open issues".into()))?;
        check_text(&input.title, &input.body)?;
        let issue = self
            .meta
            .create_issue(
                repo,
                &NewIssue {
                    title: input.title.trim().into(),
                    body: input.body.clone(),
                    author: user.name.clone(),
                    milestone: if role >= Role::Triage { input.milestone } else { None },
                    ..Default::default()
                },
            )
            .await?;
        // Readers may open issues; only triage may label and assign them.
        if role >= Role::Triage {
            if !input.labels.is_empty() {
                self.apply_labels(who, repo, issue.number, &input.labels, role).await?;
            }
            if !input.assignees.is_empty() {
                self.check_people(&input.assignees).await?;
                self.meta.set_item_people(repo, issue.number, "assignee", &input.assignees).await?;
            }
        }
        self.subscribe_participant(user.id, repo, issue.number).await;
        self.publish_issue(repo, issue.number, "opened", &user.name).await;
        Ok(self.issues_json(repo, &[issue]).await?.remove(0))
    }

    pub async fn edit_issue(&self, who: &Principal, repo: RepoId, number: i64, e: IssueEdit) -> FluxResult<IssueJson> {
        let role = self.authorize(who, repo, Role::Read).await?;
        self.issues_on(repo).await?;
        let actor = who.name().to_string();
        let issue = self.issue_or_404(repo, number).await?;
        let is_author = who.user.as_ref().is_some_and(|u| u.name == issue.author);
        let can_triage = role >= Role::Triage;
        if (e.title.is_some() || e.body.is_some() || e.state.is_some()) && !(is_author || can_triage) {
            return Err(FluxError::PermissionDenied("only the author or triagers can change this issue".into()));
        }
        if (e.labels.is_some() || e.assignees.is_some() || e.milestone.is_some() || e.locked.is_some()) && !can_triage {
            return Err(FluxError::PermissionDenied("labels, assignees, milestones and locking need triage access".into()));
        }
        if let Some(t) = &e.title {
            check_text(t, e.body.as_deref().unwrap_or(""))?;
        }
        if let Some(s) = &e.state
            && s != "open"
            && s != "closed"
        {
            return Err(FluxError::InvalidArgument("state must be open or closed".into()));
        }
        let update = IssueUpdate {
            title: e.title.as_ref().map(|t| t.trim().to_string()),
            body: e.body.clone(),
            state: e.state.clone(),
            state_reason: match (&e.state, &e.state_reason) {
                (_, Some(r)) => Some(r.clone()),
                (Some(s), None) if s == "closed" => Some("completed".into()),
                (Some(_), None) => Some(String::new()),
                _ => None,
            },
            milestone: e.milestone,
            locked: e.locked,
        };
        let updated =
            self.meta.update_issue(repo, number, &update).await?.ok_or_else(|| FluxError::NotFound("issue vanished".into()))?;
        let mut actions = Vec::new();
        if let Some(t) = &e.title
            && t.trim() != issue.title
        {
            self.meta
                .add_issue_event(repo, number, "renamed", &actor, &format!("{} → {}", issue.title, t.trim()), None, None)
                .await?;
            actions.push("edited");
        }
        if e.body.is_some() && e.title.is_none() {
            actions.push("edited");
        }
        if let Some(s) = &e.state
            && *s != issue.state
        {
            let kind = if s == "closed" { "closed" } else { "reopened" };
            self.meta.add_issue_event(repo, number, kind, &actor, &updated.state_reason, None, None).await?;
            actions.push(kind);
        }
        if let Some(m) = e.milestone
            && m != issue.milestone
        {
            let body = m.map(|n| n.to_string()).unwrap_or_default();
            self.meta
                .add_issue_event(repo, number, if m.is_some() { "milestoned" } else { "demilestoned" }, &actor, &body, None, None)
                .await?;
            actions.push("milestoned");
        }
        if let Some(l) = e.locked
            && l != issue.locked
        {
            self.meta.add_issue_event(repo, number, if l { "locked" } else { "unlocked" }, &actor, "", None, None).await?;
            actions.push(if l { "locked" } else { "unlocked" });
        }
        if let Some(labels) = &e.labels {
            self.apply_labels(who, repo, number, labels, role).await?;
            actions.push("labeled");
        }
        if let Some(people) = &e.assignees {
            self.set_people(who, repo, number, "assignee", people).await?;
            actions.push("assigned");
        }
        actions.dedup();
        for a in actions {
            self.publish_issue(repo, number, a, &actor).await;
        }
        let fresh = self.issue_or_404(repo, number).await?;
        Ok(self.issues_json(repo, &[fresh]).await?.remove(0))
    }

    pub async fn comment_issue(&self, who: &Principal, repo: RepoId, number: i64, body: &str) -> FluxResult<EventJson> {
        let role = self.authorize(who, repo, Role::Read).await?;
        self.issues_on(repo).await?;
        let user = who.user.as_ref().ok_or_else(|| FluxError::Unauthenticated("sign in to comment".into()))?;
        let issue = self.issue_or_404(repo, number).await?;
        if issue.locked && role < Role::Triage {
            return Err(FluxError::PermissionDenied("this conversation is locked".into()));
        }
        if body.trim().is_empty() {
            return Err(FluxError::InvalidArgument("empty comment".into()));
        }
        if body.len() > MAX_BODY {
            return Err(FluxError::InvalidArgument(format!("comments are limited to {MAX_BODY} bytes")));
        }
        let ev = self.meta.add_issue_event(repo, number, "comment", &user.name, body, None, None).await?;
        self.subscribe_participant(user.id, repo, number).await;
        let name = self.repo_name(repo).await;
        self.publish(Event::Comment {
            repo: name,
            number,
            on: "issue",
            seq: ev.seq,
            actor: user.name.clone(),
            body: body.into(),
        });
        Ok(EventJson::from(&ev))
    }

    /// Edit or delete (`body = None`) a comment: its author or triagers.
    pub async fn change_comment(&self, who: &Principal, repo: RepoId, seq: i64, body: Option<&str>) -> FluxResult<()> {
        let role = self.authorize(who, repo, Role::Read).await?;
        let ev = self.meta.get_issue_event(repo, seq).await?.ok_or_else(|| FluxError::NotFound("comment not found".into()))?;
        if ev.kind != "comment" {
            return Err(FluxError::InvalidArgument("only comments can be changed".into()));
        }
        let mine = who.user.as_ref().is_some_and(|u| u.name == ev.author);
        if !mine && role < Role::Triage {
            return Err(FluxError::PermissionDenied("only the author or triagers can change this comment".into()));
        }
        match body {
            Some(b) if b.trim().is_empty() => return Err(FluxError::InvalidArgument("empty comment".into())),
            Some(b) => {
                self.meta.update_issue_event(repo, seq, b).await?;
            }
            None => {
                self.meta.delete_issue_event(repo, seq).await?;
            }
        }
        Ok(())
    }

    /// Set the labels of an issue or review. Triagers apply existing labels;
    /// writers may also introduce new ones (created with a neutral color).
    pub async fn apply_labels(
        &self,
        who: &Principal,
        repo: RepoId,
        number: i64,
        labels: &[String],
        role: Role,
    ) -> FluxResult<()> {
        if role < Role::Triage {
            return Err(FluxError::PermissionDenied("labeling needs triage access".into()));
        }
        if self.meta.item_kind(repo, number).await?.is_none() {
            return Err(FluxError::NotFound(format!("#{number} not found")));
        }
        let existing = self.meta.list_labels(repo).await?;
        let mut clean: Vec<String> = Vec::new();
        for l in labels {
            let l = l.trim();
            if l.is_empty() || l.len() > 50 {
                return Err(FluxError::InvalidArgument(format!("invalid label {l:?}")));
            }
            match existing.iter().find(|e| e.name.eq_ignore_ascii_case(l)) {
                Some(e) => clean.push(e.name.clone()),
                None if role >= Role::Write => {
                    self.meta
                        .put_label(repo, &Label { name: l.into(), color: "8a8f98".into(), description: String::new() })
                        .await?;
                    clean.push(l.into());
                }
                None => return Err(FluxError::InvalidArgument(format!("no label named {l:?}"))),
            }
        }
        clean.sort();
        clean.dedup();
        let before: Vec<String> = self.meta.item_labels(repo, &[number]).await?.into_iter().map(|(_, l)| l).collect();
        self.meta.set_item_labels(repo, number, &clean).await?;
        let actor = who.name();
        if self.meta.item_kind(repo, number).await? == Some("issue") {
            for l in clean.iter().filter(|l| !before.contains(l)) {
                self.meta.add_issue_event(repo, number, "labeled", actor, l, None, None).await?;
            }
            for l in before.iter().filter(|l| !clean.contains(l)) {
                self.meta.add_issue_event(repo, number, "unlabeled", actor, l, None, None).await?;
            }
        }
        Ok(())
    }

    async fn check_people(&self, names: &[String]) -> FluxResult<()> {
        if names.len() > 10 {
            return Err(FluxError::InvalidArgument("at most 10 people".into()));
        }
        for n in names {
            if self.meta.get_user(n).await?.is_none() {
                return Err(FluxError::InvalidArgument(format!("no user named {n:?}")));
            }
        }
        Ok(())
    }

    /// Set assignees (`assignee`) or requested reviewers (`reviewer`).
    pub async fn set_people(&self, who: &Principal, repo: RepoId, number: i64, kind: &str, names: &[String]) -> FluxResult<()> {
        self.authorize(who, repo, Role::Triage).await?;
        if !matches!(kind, "assignee" | "reviewer") {
            return Err(FluxError::InvalidArgument("kind must be assignee or reviewer".into()));
        }
        let item = self.meta.item_kind(repo, number).await?.ok_or_else(|| FluxError::NotFound(format!("#{number} not found")))?;
        let mut names: Vec<String> = names.iter().map(|n| n.trim().trim_start_matches('@').to_string()).collect();
        names.sort();
        names.dedup();
        self.check_people(&names).await?;
        let before: Vec<String> = self.meta.item_people(repo, &[number], kind).await?.into_iter().map(|(_, p)| p).collect();
        self.meta.set_item_people(repo, number, kind, &names).await?;
        let added: Vec<&String> = names.iter().filter(|n| !before.contains(n)).collect();
        for n in &added {
            if item == "issue" {
                self.meta.add_issue_event(repo, number, "assigned", who.name(), n, None, None).await?;
            }
            if let Some(u) = self.meta.get_user(n).await? {
                let reason = if kind == "reviewer" { "review_requested" } else { "assigned" };
                let title = self.item_title(repo, number).await.unwrap_or_default();
                let _ = self.meta.notify(u.id, repo, number, item, reason, &title).await;
            }
        }
        if item == "issue" {
            for n in before.iter().filter(|n| !names.contains(n)) {
                self.meta.add_issue_event(repo, number, "unassigned", who.name(), n, None, None).await?;
            }
        }
        Ok(())
    }

    pub async fn item_title(&self, repo: RepoId, number: i64) -> FluxResult<String> {
        if let Some(i) = self.meta.get_issue(repo, number).await? {
            return Ok(i.title);
        }
        Ok(self.meta.get_review(repo, number).await?.map(|r| r.title).unwrap_or_default())
    }

    /// Participants are subscribed to the thread (best effort).
    pub async fn subscribe_participant(&self, user_id: i64, repo: RepoId, number: i64) {
        if let Ok(None) = self.meta.subscription(user_id, repo, number).await {
            let _ = self.meta.set_subscription(user_id, repo, number, Some("subscribed")).await;
        }
    }

    // ───────────── labels ─────────────

    pub async fn put_label(&self, who: &Principal, repo: RepoId, old: Option<&str>, label: Label) -> FluxResult<()> {
        self.authorize(who, repo, Role::Write).await?;
        let name = label.name.trim();
        if name.is_empty() || name.len() > 50 {
            return Err(FluxError::InvalidArgument("label names are 1-50 characters".into()));
        }
        let color = label.color.trim_start_matches('#').to_ascii_lowercase();
        if !valid_color(&color) {
            return Err(FluxError::InvalidArgument("colors are six hex digits, like d73a4a".into()));
        }
        let label = Label { name: name.into(), color, description: label.description.chars().take(100).collect() };
        match old {
            Some(o) => {
                if !self.meta.rename_label(repo, o, &label).await? {
                    return Err(FluxError::NotFound(format!("no label named {o:?}")));
                }
            }
            None => self.meta.put_label(repo, &label).await?,
        }
        Ok(())
    }

    pub async fn delete_label(&self, who: &Principal, repo: RepoId, name: &str) -> FluxResult<()> {
        self.authorize(who, repo, Role::Write).await?;
        if !self.meta.delete_label(repo, name).await? {
            return Err(FluxError::NotFound(format!("no label named {name:?}")));
        }
        Ok(())
    }

    /// The labels GitHub starts repositories with.
    pub async fn seed_labels(&self, repo: RepoId) -> FluxResult<()> {
        for (name, color, description) in [
            ("bug", "d73a4a", "Something isn't working"),
            ("documentation", "0075ca", "Improvements or additions to documentation"),
            ("duplicate", "cfd3d7", "This issue or review already exists"),
            ("enhancement", "a2eeef", "New feature or request"),
            ("good first issue", "7057ff", "Good for newcomers"),
            ("help wanted", "008672", "Extra attention is needed"),
            ("question", "d876e3", "Further information is requested"),
            ("wontfix", "ffffff", "This will not be worked on"),
        ] {
            self.meta.put_label(repo, &Label { name: name.into(), color: color.into(), description: description.into() }).await?;
        }
        Ok(())
    }

    // ───────────── milestones ─────────────

    pub async fn create_milestone(
        &self,
        who: &Principal,
        repo: RepoId,
        title: &str,
        description: &str,
        due_ms: Option<i64>,
    ) -> FluxResult<Milestone> {
        self.authorize(who, repo, Role::Write).await?;
        if title.trim().is_empty() {
            return Err(FluxError::InvalidArgument("a milestone needs a title".into()));
        }
        Ok(self.meta.create_milestone(repo, None, title.trim(), description, due_ms).await?)
    }

    pub async fn update_milestone(
        &self,
        who: &Principal,
        repo: RepoId,
        number: i64,
        title: &str,
        description: &str,
        state: &str,
        due_ms: Option<i64>,
    ) -> FluxResult<()> {
        self.authorize(who, repo, Role::Write).await?;
        if !matches!(state, "open" | "closed") {
            return Err(FluxError::InvalidArgument("state must be open or closed".into()));
        }
        if !self.meta.update_milestone(repo, number, title, description, state, due_ms).await? {
            return Err(FluxError::NotFound(format!("milestone {number} not found")));
        }
        Ok(())
    }

    pub async fn delete_milestone(&self, who: &Principal, repo: RepoId, number: i64) -> FluxResult<()> {
        self.authorize(who, repo, Role::Write).await?;
        if !self.meta.delete_milestone(repo, number).await? {
            return Err(FluxError::NotFound(format!("milestone {number} not found")));
        }
        Ok(())
    }
}

/// `@name` and `@org/team` mentions in Markdown text (outside code).
pub fn mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || line.starts_with("    ") || line.trim_start().starts_with('>') {
            continue;
        }
        let mut in_code = false;
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'`' => in_code = !in_code,
                b'@' if !in_code && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'.')) => {
                    let start = i + 1;
                    let mut j = start;
                    while j < b.len() && (b[j].is_ascii_alphanumeric() || b"-_./".contains(&b[j])) {
                        j += 1;
                    }
                    let name = line[start..j].trim_end_matches(['.', '/']);
                    if !name.is_empty() && name.len() <= 80 && !out.iter().any(|n| n == name) {
                        out.push(name.to_string());
                    }
                    i = j;
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_mentions_outside_code() {
        assert_eq!(mentions("cc @ann and @acme/core."), vec!["ann", "acme/core"]);
        assert_eq!(mentions("mail me at a@b.com, `@notme`"), Vec::<String>::new());
        assert_eq!(mentions("```\n@nope\n```\n> @quoted\n@yes"), vec!["yes"]);
        assert_eq!(mentions("@ann @ann"), vec!["ann"]);
    }
}
