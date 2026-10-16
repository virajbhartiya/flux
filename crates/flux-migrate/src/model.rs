//! The parts of GitHub's REST API responses the migration reads. Every
//! field defaults, so fields missing on older GitHub Enterprise versions
//! do not fail a migration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct User {
    pub login: String,
    pub id: u64,
    #[serde(rename = "type")]
    pub kind: String,
}

/// `GET /users/{login}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub login: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Repo {
    pub name: String,
    pub full_name: String,
    pub owner: User,
    pub description: Option<String>,
    pub default_branch: String,
    pub homepage: Option<String>,
    pub topics: Vec<String>,
    pub visibility: Option<String>,
    pub private: bool,
    pub archived: bool,
    pub fork: bool,
    pub has_issues: bool,
    pub has_pages: bool,
    pub has_discussions: bool,
    pub html_url: String,
    pub clone_url: String,
}

impl Repo {
    pub fn visibility(&self) -> &str {
        match self.visibility.as_deref() {
            Some(v @ ("public" | "private" | "internal")) => v,
            _ if self.private => "private",
            _ => "public",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Label {
    pub name: String,
    pub color: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Milestone {
    pub number: i64,
    pub title: String,
    pub description: Option<String>,
    pub state: String,
    pub due_on: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MilestoneRef {
    pub number: i64,
}

/// An issue, or a pull request as `/issues` also lists them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Issue {
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub user: Option<User>,
    pub state: String,
    pub state_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub labels: Vec<Label>,
    pub assignees: Vec<User>,
    pub milestone: Option<MilestoneRef>,
    pub locked: bool,
    /// Present when the item is a pull request.
    pub pull_request: Option<serde_json::Value>,
}

/// A comment on an issue or on a pull request's conversation.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct IssueComment {
    pub id: u64,
    pub user: Option<User>,
    pub body: Option<String>,
    pub created_at: String,
    pub issue_url: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PullRef {
    pub sha: String,
    #[serde(rename = "ref")]
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Pull {
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub user: Option<User>,
    pub state: String,
    pub draft: bool,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub head: PullRef,
    pub base: PullRef,
    pub labels: Vec<Label>,
    pub assignees: Vec<User>,
    pub requested_reviewers: Vec<User>,
}

/// A line comment on a pull request's diff.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReviewComment {
    pub id: u64,
    pub user: Option<User>,
    pub body: Option<String>,
    pub created_at: String,
    pub path: String,
    pub line: Option<i64>,
    pub original_line: Option<i64>,
    /// `LEFT` (the old side of the diff) or `RIGHT`.
    pub side: Option<String>,
    pub pull_request_url: String,
}

/// A submitted pull request review.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Review {
    pub id: u64,
    pub user: Option<User>,
    pub body: Option<String>,
    /// `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED`, `PENDING`.
    pub state: String,
    pub submitted_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Release {
    pub id: u64,
    pub tag_name: String,
    pub name: Option<String>,
    pub body: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub author: Option<User>,
    pub created_at: String,
    pub published_at: Option<String>,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Asset {
    pub id: u64,
    pub name: String,
    pub size: u64,
    pub content_type: String,
    /// API URL; with `Accept: application/octet-stream` it serves the file.
    pub url: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Hook {
    pub id: u64,
    pub active: bool,
    pub events: Vec<String>,
    pub config: HookConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HookConfig {
    pub url: Option<String>,
}

/// `GET …/actions/secrets` and `…/actions/variables`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Named {
    pub secrets: Vec<Name>,
    pub variables: Vec<Name>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Name {
    pub name: String,
}
