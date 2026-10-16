//! GitHub objects → the flux bulk import format (`POST …/import`), link
//! rewriting, and branch protection mapping.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::model::{self, Issue, IssueComment, Pull, Review, ReviewComment, User};
use crate::time::parse_rfc3339;

#[derive(Debug, Default, Serialize)]
pub struct ImportBatch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<UserIn>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<LabelIn>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub milestones: Vec<MilestoneIn>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<IssueIn>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reviews: Vec<ReviewIn>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub releases: Vec<ReleaseIn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UserIn {
    pub login: String,
    pub name: String,
    pub provider: Option<String>,
    pub subject: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LabelIn {
    pub name: String,
    pub color: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MilestoneIn {
    pub number: i64,
    pub title: String,
    pub description: String,
    pub state: String,
    pub due_on: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommentIn {
    pub id: String,
    pub author: String,
    pub body: String,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IssueIn {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub author: String,
    pub state: String,
    pub state_reason: String,
    pub created_at: i64,
    pub updated_at: Option<i64>,
    pub closed_at: Option<i64>,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub milestone: Option<i64>,
    pub locked: bool,
    pub comments: Vec<CommentIn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewIn {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub author: String,
    /// `open`, `closed` or `landed`.
    pub state: String,
    pub head: String,
    pub target: String,
    pub merged: Option<String>,
    pub draft: bool,
    pub created_at: i64,
    pub updated_at: Option<i64>,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub reviewers: Vec<String>,
    pub comments: Vec<CommentIn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseIn {
    pub tag: String,
    pub name: String,
    pub body: String,
    pub draft: bool,
    pub prerelease: bool,
    pub author: String,
    pub created_at: i64,
    pub published_at: Option<i64>,
}

/// Rewrites absolute links to the GitHub repository's issues and pull
/// requests into links to the flux repository.
#[derive(Debug, Clone)]
pub struct Links {
    /// `https://github.com/owner/repo`, lowercase.
    from: String,
    /// `https://flux.example.com/owner/repo`.
    to: String,
}

impl Links {
    pub fn new(github_html_url: &str, flux_html_url: &str) -> Self {
        Self {
            from: github_html_url.trim_end_matches('/').to_ascii_lowercase(),
            to: flux_html_url.trim_end_matches('/').to_string(),
        }
    }

    /// `…/issues/N` → `<flux>/-/issues/N`, `…/pull/N` → `<flux>/-/reviews/N`;
    /// anything after the number (fragments, sub-pages) is kept.
    pub fn rewrite(&self, text: &str) -> String {
        if self.from.is_empty() {
            return text.to_string();
        }
        // ASCII lowercasing keeps byte offsets, so matches index `text` too.
        let lower = text.to_ascii_lowercase();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while let Some(off) = lower[i..].find(&self.from) {
            let start = i + off;
            let after = start + self.from.len();
            out.push_str(&text[i..start]);
            let rest = &lower[after..];
            let found = [("/issues/", "issues"), ("/pull/", "reviews")].into_iter().find_map(|(p, kind)| {
                let tail = rest.strip_prefix(p)?;
                let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
                (digits > 0).then(|| (kind, p.len(), &tail[..digits]))
            });
            match found {
                Some((kind, plen, n)) => {
                    out.push_str(&format!("{}/-/{kind}/{n}", self.to));
                    i = after + plen + n.len();
                }
                None => {
                    out.push_str(&text[start..after]);
                    i = after;
                }
            }
        }
        out.push_str(&text[i..]);
        out
    }
}

/// Converts GitHub objects, optionally rewriting links in text.
#[derive(Debug, Clone, Default)]
pub struct Converter {
    pub links: Option<Links>,
}

/// The login of a user, `ghost` for deleted accounts (as GitHub shows them).
pub fn login(u: Option<&User>) -> String {
    u.map(|u| u.login.clone()).filter(|l| !l.is_empty()).unwrap_or_else(|| "ghost".into())
}

fn ms(s: &str) -> i64 {
    parse_rfc3339(s).unwrap_or(0)
}

fn opt_ms(s: Option<&String>) -> Option<i64> {
    s.and_then(|s| parse_rfc3339(s))
}

/// The number at the end of an API URL (`…/issues/12`, `…/pulls/7`).
pub fn trailing_number(url: &str) -> Option<i64> {
    url.trim_end_matches('/').rsplit('/').next()?.parse().ok()
}

/// A line comment's line: `line` (or `original_line` once outdated),
/// negative on the diff's old (`LEFT`) side.
pub fn comment_line(c: &ReviewComment) -> Option<i64> {
    let n = c.line.or(c.original_line)?;
    Some(if c.side.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("LEFT")) { -n } else { n })
}

/// The flux comment kind of a submitted review, or `None` for reviews that
/// carry nothing on their own (empty `COMMENTED` containers of line comments,
/// pending drafts).
pub fn review_kind(state: &str, body: &str) -> Option<&'static str> {
    match state {
        "APPROVED" => Some("approved"),
        "CHANGES_REQUESTED" => Some("changes_requested"),
        "COMMENTED" | "DISMISSED" if !body.trim().is_empty() => Some("comment"),
        _ => None,
    }
}

/// `(issues, pull request numbers)`: `/issues` lists pull requests too.
pub fn split_issues(items: Vec<Issue>) -> (Vec<Issue>, BTreeSet<i64>) {
    let (pulls, issues): (Vec<Issue>, Vec<Issue>) = items.into_iter().partition(|i| i.pull_request.is_some());
    (issues, pulls.into_iter().map(|p| p.number).collect())
}

fn names(labels: &[model::Label]) -> Vec<String> {
    labels.iter().map(|l| l.name.clone()).collect()
}

fn logins(users: &[User]) -> Vec<String> {
    users.iter().map(|u| u.login.clone()).filter(|l| !l.is_empty()).collect()
}

fn sorted(mut v: Vec<CommentIn>) -> Vec<CommentIn> {
    v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
    v
}

impl Converter {
    pub fn text(&self, s: Option<&str>) -> String {
        let s = s.unwrap_or_default();
        match &self.links {
            Some(l) => l.rewrite(s),
            None => s.to_string(),
        }
    }

    pub fn label(&self, l: &model::Label) -> LabelIn {
        LabelIn { name: l.name.clone(), color: l.color.clone(), description: l.description.clone().unwrap_or_default() }
    }

    pub fn milestone(&self, m: &model::Milestone) -> MilestoneIn {
        MilestoneIn {
            number: m.number,
            title: m.title.clone(),
            description: self.text(m.description.as_deref()),
            state: if m.state == "closed" { "closed".into() } else { "open".into() },
            due_on: opt_ms(m.due_on.as_ref()),
        }
    }

    fn issue_comment(&self, c: &IssueComment) -> CommentIn {
        CommentIn {
            id: format!("ic:{}", c.id),
            author: login(c.user.as_ref()),
            body: self.text(c.body.as_deref()),
            created_at: ms(&c.created_at),
            path: None,
            line: None,
            kind: None,
        }
    }

    pub fn issue(&self, i: &Issue, comments: &[IssueComment]) -> IssueIn {
        IssueIn {
            number: i.number,
            title: i.title.clone(),
            body: self.text(i.body.as_deref()),
            author: login(i.user.as_ref()),
            state: if i.state == "closed" { "closed".into() } else { "open".into() },
            state_reason: i.state_reason.clone().unwrap_or_default(),
            created_at: ms(&i.created_at),
            updated_at: parse_rfc3339(&i.updated_at),
            closed_at: opt_ms(i.closed_at.as_ref()),
            labels: names(&i.labels),
            assignees: logins(&i.assignees),
            milestone: i.milestone.as_ref().map(|m| m.number),
            locked: i.locked,
            comments: sorted(comments.iter().map(|c| self.issue_comment(c)).collect()),
        }
    }

    /// A pull request as a flux review, with its conversation comments,
    /// line comments and submitted reviews as one timeline.
    pub fn review(&self, p: &Pull, conversation: &[IssueComment], lines: &[ReviewComment], reviews: &[Review]) -> ReviewIn {
        let mut comments: Vec<CommentIn> = conversation.iter().map(|c| self.issue_comment(c)).collect();
        for c in lines {
            comments.push(CommentIn {
                id: format!("rc:{}", c.id),
                author: login(c.user.as_ref()),
                body: self.text(c.body.as_deref()),
                created_at: ms(&c.created_at),
                path: Some(c.path.clone()).filter(|p| !p.is_empty()),
                line: comment_line(c),
                kind: None,
            });
        }
        let mut reviewers: Vec<String> = logins(&p.requested_reviewers);
        for r in reviews {
            let body = r.body.as_deref().unwrap_or_default();
            let Some(kind) = review_kind(&r.state, body) else { continue };
            let author = login(r.user.as_ref());
            if !reviewers.contains(&author) && author != login(p.user.as_ref()) {
                reviewers.push(author.clone());
            }
            comments.push(CommentIn {
                id: format!("rv:{}", r.id),
                author,
                body: self.text(Some(body)),
                created_at: r.submitted_at.as_deref().map(ms).unwrap_or_else(|| ms(&p.updated_at)),
                path: None,
                line: None,
                kind: Some(kind.into()),
            });
        }
        let merged = p.merged_at.is_some();
        ReviewIn {
            number: p.number,
            title: p.title.clone(),
            body: self.text(p.body.as_deref()),
            author: login(p.user.as_ref()),
            state: if merged {
                "landed".into()
            } else if p.state == "closed" {
                "closed".into()
            } else {
                "open".into()
            },
            head: p.head.sha.clone(),
            target: p.base.name.clone(),
            merged: if merged { p.merge_commit_sha.clone().filter(|s| !s.is_empty()) } else { None },
            draft: p.draft,
            created_at: ms(&p.created_at),
            updated_at: [p.merged_at.as_ref(), p.closed_at.as_ref()]
                .into_iter()
                .flatten()
                .find_map(|s| parse_rfc3339(s))
                .or_else(|| parse_rfc3339(&p.updated_at)),
            labels: names(&p.labels),
            assignees: logins(&p.assignees),
            reviewers,
            comments: sorted(comments),
        }
    }

    pub fn release(&self, r: &model::Release) -> ReleaseIn {
        ReleaseIn {
            tag: r.tag_name.clone(),
            name: r.name.clone().unwrap_or_default(),
            body: self.text(r.body.as_deref()),
            draft: r.draft,
            prerelease: r.prerelease,
            author: login(r.author.as_ref()),
            created_at: ms(&r.created_at),
            published_at: opt_ms(r.published_at.as_ref()),
        }
    }
}

/// Group comments by the issue or pull request number at the end of `url`.
pub fn by_number<T>(items: Vec<T>, url: impl Fn(&T) -> &str) -> BTreeMap<i64, Vec<T>> {
    let mut out: BTreeMap<i64, Vec<T>> = BTreeMap::new();
    for item in items {
        if let Some(n) = trailing_number(url(&item)) {
            out.entry(n).or_default().push(item);
        }
    }
    out
}

/// flux's branch rule (`PUT …/protection`). Unknown fields round-trip.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BranchRule {
    pub pattern: String,
    pub require_review: bool,
    pub required_approvals: u32,
    pub require_codeowners: bool,
    pub dismiss_stale_approvals: bool,
    pub required_checks: Vec<String>,
    pub allow_force_push: bool,
    pub allow_deletion: bool,
    pub push_allow: Vec<String>,
    pub enforce_admins: bool,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

/// flux's built-in rules, which a repository without configured rules uses.
pub fn default_rules() -> Vec<BranchRule> {
    ["main", "master", "trunk", "release/*"].into_iter().map(|p| BranchRule { pattern: p.into(), ..Default::default() }).collect()
}

/// GitHub branch protection (`GET …/branches/{b}/protection`) → a flux rule.
/// Status check names stay the same (flux CI reports `<workflow> / <job>`).
pub fn protection_rule(branch: &str, p: &serde_json::Value) -> BranchRule {
    let enabled = |k: &str| p[k]["enabled"].as_bool().unwrap_or(false);
    let reviews = &p["required_pull_request_reviews"];
    let mut checks: Vec<String> = p["required_status_checks"]["contexts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c.as_str().map(str::to_string))
        .collect();
    for c in p["required_status_checks"]["checks"].as_array().into_iter().flatten() {
        if let Some(name) = c["context"].as_str()
            && !checks.iter().any(|x| x == name)
        {
            checks.push(name.to_string());
        }
    }
    BranchRule {
        pattern: branch.to_string(),
        require_review: reviews.is_object(),
        required_approvals: reviews["required_approving_review_count"].as_u64().unwrap_or(0).min(10) as u32,
        require_codeowners: reviews["require_code_owner_reviews"].as_bool().unwrap_or(false),
        dismiss_stale_approvals: reviews["dismiss_stale_reviews"].as_bool().unwrap_or(false),
        required_checks: checks,
        allow_force_push: enabled("allow_force_pushes"),
        allow_deletion: enabled("allow_deletions"),
        push_allow: Vec::new(),
        enforce_admins: enabled("enforce_admins"),
        other: Default::default(),
    }
}

/// `rules` with `rule` replacing the one for the same pattern.
pub fn upsert_rule(rules: Option<Vec<BranchRule>>, rule: BranchRule) -> Vec<BranchRule> {
    let mut rules = rules.unwrap_or_else(default_rules);
    match rules.iter_mut().find(|r| r.pattern == rule.pattern) {
        Some(r) => *r = BranchRule { push_allow: r.push_allow.clone(), other: r.other.clone(), ..rule },
        None => rules.push(rule),
    }
    rules
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn user(login: &str) -> Option<User> {
        Some(User { login: login.into(), id: 1, kind: "User".into() })
    }

    fn pull(state: &str, merged: bool, draft: bool) -> Pull {
        Pull {
            number: 7,
            title: "Add widgets".into(),
            body: Some("Fixes https://github.com/Acme/Widget/issues/3".into()),
            user: user("ada"),
            state: state.into(),
            draft,
            created_at: "2024-01-01T00:00:00Z".into(),
            updated_at: "2024-01-03T00:00:00Z".into(),
            closed_at: (state == "closed").then(|| "2024-01-02T00:00:00Z".into()),
            merged_at: merged.then(|| "2024-01-02T00:00:00Z".into()),
            merge_commit_sha: Some("m".repeat(40)),
            head: model::PullRef { sha: "h".repeat(40), name: "feature".into() },
            base: model::PullRef { sha: String::new(), name: "main".into() },
            labels: vec![model::Label { name: "enhancement".into(), ..Default::default() }],
            assignees: vec![User { login: "bob".into(), ..Default::default() }],
            requested_reviewers: vec![User { login: "cy".into(), ..Default::default() }],
        }
    }

    #[test]
    fn pull_request_states() {
        let c = Converter::default();
        let r = c.review(&pull("closed", true, false), &[], &[], &[]);
        assert_eq!(r.state, "landed");
        assert_eq!(r.merged.as_deref(), Some("m".repeat(40).as_str()));
        assert_eq!(r.updated_at, parse_rfc3339("2024-01-02T00:00:00Z"));
        let r = c.review(&pull("closed", false, false), &[], &[], &[]);
        assert_eq!((r.state.as_str(), r.merged.as_deref()), ("closed", None));
        let r = c.review(&pull("open", false, true), &[], &[], &[]);
        assert_eq!(r.state, "open");
        assert!(r.draft);
        assert_eq!(r.merged, None, "open pulls' merge_commit_sha is a test merge");
        assert_eq!((r.head.as_str(), r.target.as_str()), ("h".repeat(40).as_str(), "main"));
        assert_eq!(r.labels, ["enhancement"]);
        assert_eq!(r.assignees, ["bob"]);
        assert_eq!(r.reviewers, ["cy"]);
    }

    #[test]
    fn pull_request_timeline() {
        let c = Converter::default();
        let conv = [IssueComment {
            id: 1,
            user: user("bob"),
            body: Some("looks good".into()),
            created_at: "2024-01-01T01:00:00Z".into(),
            issue_url: "https://api.github.com/repos/acme/widget/issues/7".into(),
        }];
        let line = |id, line, original, side: &str| ReviewComment {
            id,
            user: user("cy"),
            body: Some(format!("c{id}")),
            created_at: "2024-01-01T02:00:00Z".into(),
            path: "src/lib.rs".into(),
            line,
            original_line: original,
            side: Some(side.into()),
            pull_request_url: "https://api.github.com/repos/acme/widget/pulls/7".into(),
        };
        let lines = [line(10, Some(5), Some(4), "RIGHT"), line(11, Some(9), None, "LEFT"), line(12, None, Some(3), "RIGHT")];
        let review = |id, state: &str, body: &str, at: &str| Review {
            id,
            user: user("dee"),
            body: Some(body.into()),
            state: state.into(),
            submitted_at: Some(at.into()),
        };
        let reviews = [
            review(20, "COMMENTED", "", "2024-01-01T02:00:01Z"),
            review(21, "CHANGES_REQUESTED", "please fix", "2024-01-01T03:00:00Z"),
            review(22, "APPROVED", "", "2024-01-01T04:00:00Z"),
            review(23, "COMMENTED", "a note", "2024-01-01T00:30:00Z"),
            review(24, "PENDING", "draft", "2024-01-01T05:00:00Z"),
        ];
        let r = c.review(&pull("open", false, false), &conv, &lines, &reviews);
        let got: Vec<(&str, Option<&str>, Option<i64>)> =
            r.comments.iter().map(|c| (c.id.as_str(), c.kind.as_deref(), c.line)).collect();
        assert_eq!(
            got,
            [
                ("rv:23", Some("comment"), None),
                ("ic:1", None, None),
                ("rc:10", None, Some(5)),
                ("rc:11", None, Some(-9)),
                ("rc:12", None, Some(3)),
                ("rv:21", Some("changes_requested"), None),
                ("rv:22", Some("approved"), None),
            ]
        );
        assert_eq!(r.comments[2].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(r.reviewers, ["cy", "dee"]);
    }

    #[test]
    fn issues_and_pulls_are_separated() {
        let item = |n, pr: bool| Issue { number: n, pull_request: pr.then(|| json!({"url": "x"})), ..Default::default() };
        let (issues, pulls) = split_issues(vec![item(1, false), item(2, true), item(3, false), item(4, true)]);
        assert_eq!(issues.iter().map(|i| i.number).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(pulls.into_iter().collect::<Vec<_>>(), [2, 4]);
        let grouped = by_number(
            vec![("https://api.github.com/repos/a/b/issues/3", 1), ("https://api.github.com/repos/a/b/issues/3", 2), ("bad", 3)],
            |c| c.0,
        );
        assert_eq!(grouped[&3].len(), 2);
        assert_eq!(grouped.len(), 1);
    }

    #[test]
    fn issue_fields() {
        let c = Converter::default();
        let i = Issue {
            number: 3,
            title: "Broken".into(),
            body: None,
            user: None,
            state: "closed".into(),
            state_reason: Some("not_planned".into()),
            created_at: "2024-01-01T00:00:00Z".into(),
            updated_at: "2024-01-02T00:00:00Z".into(),
            closed_at: Some("2024-01-02T00:00:00Z".into()),
            labels: vec![model::Label { name: "bug".into(), ..Default::default() }],
            milestone: Some(model::MilestoneRef { number: 2 }),
            locked: true,
            ..Default::default()
        };
        let x = c.issue(&i, &[]);
        assert_eq!(x.author, "ghost");
        assert_eq!((x.state.as_str(), x.state_reason.as_str()), ("closed", "not_planned"));
        assert_eq!(x.created_at, 1_704_067_200_000);
        assert_eq!(x.closed_at, Some(1_704_153_600_000));
        assert_eq!((x.milestone, x.locked, x.labels.clone()), (Some(2), true, vec!["bug".to_string()]));
    }

    #[test]
    fn links_are_rewritten() {
        let l = Links::new("https://github.com/acme/widget", "https://flux.example.com/acme/widget/");
        let text = "See https://github.com/Acme/Widget/issues/12, https://github.com/acme/widget/pull/7#discussion_r1 \
                    and https://github.com/acme/widget/pull/8/files. Not https://github.com/acme/widget-cli/issues/1, \
                    https://github.com/acme/widget/issues or https://github.com/acme/widget/wiki. é https://github.com/acme/widget/issues/3";
        assert_eq!(
            l.rewrite(text),
            "See https://flux.example.com/acme/widget/-/issues/12, https://flux.example.com/acme/widget/-/reviews/7#discussion_r1 \
             and https://flux.example.com/acme/widget/-/reviews/8/files. Not https://github.com/acme/widget-cli/issues/1, \
             https://github.com/acme/widget/issues or https://github.com/acme/widget/wiki. é https://flux.example.com/acme/widget/-/issues/3"
        );
        let c = Converter { links: Some(l) };
        assert_eq!(
            c.review(&pull("open", false, false), &[], &[], &[]).body,
            "Fixes https://flux.example.com/acme/widget/-/issues/3"
        );
    }

    #[test]
    fn protection_maps_to_a_rule() {
        let p = json!({
            "required_status_checks": { "strict": true, "contexts": ["ci/test"], "checks": [{ "context": "ci/test" }, { "context": "lint" }] },
            "enforce_admins": { "enabled": true },
            "required_pull_request_reviews": { "dismiss_stale_reviews": true, "require_code_owner_reviews": true, "required_approving_review_count": 2 },
            "allow_force_pushes": { "enabled": false },
            "allow_deletions": { "enabled": true },
        });
        let r = protection_rule("main", &p);
        assert!(r.require_review && r.require_codeowners && r.dismiss_stale_approvals && r.enforce_admins && r.allow_deletion);
        assert!(!r.allow_force_push);
        assert_eq!(r.required_approvals, 2);
        assert_eq!(r.required_checks, ["ci/test", "lint"]);
        let none = protection_rule("main", &json!({ "enforce_admins": { "enabled": false } }));
        assert!(!none.require_review);
        let rules = upsert_rule(None, r.clone());
        assert_eq!(rules.iter().map(|r| r.pattern.as_str()).collect::<Vec<_>>(), ["main", "master", "trunk", "release/*"]);
        assert_eq!(rules[0], r);
        let custom = vec![BranchRule { pattern: "main".into(), push_allow: vec!["bot".into()], ..Default::default() }];
        assert_eq!(upsert_rule(Some(custom), r)[0].push_allow, ["bot"]);
    }
}
