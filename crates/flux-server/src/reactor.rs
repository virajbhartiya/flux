//! The background reactor: turns events into webhook deliveries,
//! notifications, review requests and CI runs.
//!
//! Events reach it through an unbounded in-process channel (see
//! `Flux::publish`), so bursts are never dropped; handling is sequential
//! per server, which keeps notification and delivery order stable.

use std::collections::BTreeMap;
use std::sync::Arc;

use flux_store::RepoId;

use crate::core::{Event, Flux, FluxResult};
use crate::rbac::Role;

impl Flux {
    /// Start the reactor and its workers (idempotent).
    pub fn start_background(self: &Arc<Self>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        if self.reactor.set(tx).is_err() {
            return;
        }
        crate::hooks::start_worker(self.clone());
        crate::ci::start_scheduler(self.clone());
        let flux = self.clone();
        tokio::spawn(async move {
            while let Some(e) = rx.recv().await {
                if let Err(err) = flux.react(&e).await {
                    tracing::warn!(error = %err, repo = e.repo(), "reactor: event handling failed");
                }
            }
        });
    }

    async fn react(self: &Arc<Self>, e: &Event) -> FluxResult<()> {
        if let Some((name, repo, payload)) = self.hook_payload(e).await? {
            self.fan_out_hooks(repo, name, &payload).await?;
        }
        let Ok(repo) = self.repo(e.repo()).await else { return Ok(()) };
        match e {
            Event::Review { number, kind, actor, .. } => {
                if kind == "opened" || kind == "updated" {
                    self.request_code_owners(repo, *number, actor).await?;
                }
                self.notify_thread(repo, *number, "review", actor, None, kind == "opened").await?;
            }
            Event::Issue { number, action, actor, .. } => {
                if !matches!(action.as_str(), "labeled" | "milestoned") {
                    let body = match action.as_str() {
                        "opened" => self.meta.get_issue(repo, *number).await?.map(|i| i.body),
                        _ => None,
                    };
                    self.notify_thread(repo, *number, "issue", actor, body.as_deref(), action == "opened").await?;
                }
            }
            Event::Comment { number, on, actor, body, .. } => {
                self.notify_thread(repo, *number, on, actor, Some(body), false).await?;
            }
            _ => {}
        }
        crate::ci::on_event(self, repo, e).await?;
        Ok(())
    }

    /// Notify everyone following a thread: its subscribers, people
    /// @mentioned in `body` (who are subscribed from then on) and, for new
    /// threads, the repository's watchers. The actor is never notified.
    async fn notify_thread(
        &self,
        repo: RepoId,
        number: i64,
        kind: &str,
        actor: &str,
        body: Option<&str>,
        new_thread: bool,
    ) -> FluxResult<()> {
        let title = self.item_title(repo, number).await?;
        let mut recipients: BTreeMap<i64, &'static str> = BTreeMap::new();
        let mut ignored = Vec::new();
        for (uid, n, state) in self.meta.subscribers(repo, number).await? {
            match (n, state.as_str()) {
                (_, "ignored") => ignored.push(uid),
                (0, _) if new_thread => {
                    recipients.entry(uid).or_insert("subscribed");
                }
                (0, _) => {}
                _ => {
                    recipients.entry(uid).or_insert("subscribed");
                }
            }
        }
        if kind == "review"
            && let Some(r) = self.meta.get_review(repo, number).await?
            && let Some(u) = self.meta.get_user(&r.author).await?
        {
            recipients.entry(u.id).or_insert("author");
        }
        if let Some(text) = body {
            for m in crate::issues::mentions(text) {
                let mut users = Vec::new();
                if let Some((org, team)) = m.split_once('/') {
                    if let Ok((_, t)) = self.team_named(org, team).await {
                        users.extend(self.meta.team_members(t.id).await?.into_iter().take(100));
                    }
                } else if let Some(u) = self.meta.get_user(&m).await? {
                    users.push(u);
                }
                for u in users {
                    // Never leak private repositories through mentions.
                    if self.role_of_name(repo, &u.name).await? >= Role::Read {
                        recipients.insert(u.id, "mention");
                        self.subscribe_participant(u.id, repo, number).await;
                    }
                }
            }
        }
        let actor_id = self.meta.get_user(actor).await?.map(|u| u.id);
        for (uid, reason) in recipients {
            if Some(uid) == actor_id || ignored.contains(&uid) {
                continue;
            }
            self.meta.notify(uid, repo, number, kind, reason, &title).await?;
        }
        Ok(())
    }

    /// Request reviews from the code owners of the files a review changes.
    async fn request_code_owners(&self, repo: RepoId, number: i64, actor: &str) -> FluxResult<()> {
        let detail = self.review_detail(repo, number).await?;
        let at = detail.target_head.unwrap_or(detail.review.head);
        let owners = self.codeowners(repo, &at).await?;
        if owners.is_empty() {
            return Ok(());
        }
        let mut want: Vec<String> = Vec::new();
        for l in &detail.stack {
            for p in l.changeset.file_changes.keys() {
                for o in owners.owners_of(&p.to_string()) {
                    let o = o.trim_start_matches('@').to_string();
                    if o.contains('/') {
                        // Teams: request each member.
                        if let Some((org, team)) = o.split_once('/')
                            && let Ok((_, t)) = self.team_named(org, team).await
                        {
                            want.extend(self.meta.team_members(t.id).await?.into_iter().map(|u| u.name));
                        }
                    } else if !o.contains('@') {
                        want.push(o);
                    }
                }
            }
        }
        want.retain(|n| n != &detail.review.author && n != actor);
        want.sort();
        want.dedup();
        want.truncate(15);
        if want.is_empty() {
            return Ok(());
        }
        let mut current: Vec<String> =
            self.meta.item_people(repo, &[number], "reviewer").await?.into_iter().map(|(_, n)| n).collect();
        let before = current.len();
        for w in want {
            if !current.contains(&w) && self.meta.get_user(&w).await?.is_some() {
                current.push(w.clone());
                if let Some(u) = self.meta.get_user(&w).await? {
                    self.meta.notify(u.id, repo, number, "review", "review_requested", &detail.review.title).await?;
                }
            }
        }
        if current.len() != before {
            current.sort();
            self.meta.set_item_people(repo, number, "reviewer", &current).await?;
        }
        Ok(())
    }
}
