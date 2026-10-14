//! Landing and reviews (SPEC §7.3, §11).
//!
//! `land` moves a target ref to include a stack of changesets. If the target
//! is an ancestor of the stack it fast-forwards; otherwise the stack is
//! rebased server-side onto the current head. A rebase never needs a
//! checkout: each changeset's file changes are re-applied on the new parent,
//! and only paths the other side also changed need attention. Those are
//! resolved by a line-level three-way merge when both sides are text, and
//! reported as `REBASE_CONFLICT` otherwise. The final ref move is a
//! compare-and-swap, retried if the target moves meanwhile.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use flux_core::{Changeset, ChangesetId, FileChange, FileKind, RepoPath, Timestamp, TreeEntry};
use flux_store::{CasOutcome, RefExpect, RepoId, Review, ReviewEvent, ReviewUpdate};

use crate::core::{Event, Flux, FluxError, FluxResult, Loaded, validate_ref_name};

#[derive(Debug, Clone)]
pub struct LandResult {
    pub head: ChangesetId,
    pub rebased: Vec<ChangesetId>,
    pub fast_forward: bool,
    pub merged_files: u32,
}

pub struct ReviewDetail {
    pub review: Review,
    pub events: Vec<ReviewEvent>,
    /// Newest first.
    pub stack: Vec<Arc<Loaded>>,
    pub target_head: Option<ChangesetId>,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// What a changeset sets a path to, comparable with tree entries.
fn change_value(c: &FileChange) -> Option<(FileKind, [u8; 32])> {
    match c {
        FileChange::Change { content_id, kind, .. } => Some(match kind {
            FileKind::GitLink(_) => (*kind, [0; 32]),
            _ => (*kind, *content_id.as_bytes()),
        }),
        FileChange::Deletion | FileChange::Retain => None,
    }
}

fn entry_value(e: &Option<TreeEntry>) -> Option<(FileKind, [u8; 32])> {
    e.as_ref().and_then(|e| e.file_kind().map(|k| (k, e.id)))
}

impl Flux {
    pub async fn land(
        self: &Arc<Self>,
        repo: RepoId,
        top: ChangesetId,
        target: &[u8],
        actor: &str,
        reason: &str,
    ) -> FluxResult<LandResult> {
        validate_ref_name(target)?;
        self.load_one_arc(repo, &top).await?;
        for _attempt in 0..5 {
            let Some(cur) = self.meta.get_ref(repo, target).await? else {
                let (outcome, _) = self.update_ref(repo, target, RefExpect::Absent, Some(top), actor, reason.into()).await?;
                if !matches!(outcome, CasOutcome::Mismatch { .. }) {
                    return Ok(LandResult { head: top, rebased: vec![], fast_forward: true, merged_files: 0 });
                }
                continue;
            };
            let head = cur.target;
            if self.is_ancestor(repo, &top, &head).await? {
                // Already landed.
                return Ok(LandResult { head, rebased: vec![], fast_forward: true, merged_files: 0 });
            }
            if self.is_ancestor(repo, &head, &top).await? {
                let (outcome, _) =
                    self.update_ref(repo, target, RefExpect::Version(cur.version), Some(top), actor, reason.into()).await?;
                if matches!(outcome, CasOutcome::Mismatch { .. }) {
                    continue;
                }
                return Ok(LandResult { head: top, rebased: vec![], fast_forward: true, merged_files: 0 });
            }
            let (rebased, merged_files) = self.rebase_stack(repo, top, head, actor).await?;
            let new_head = *rebased.last().expect("non-empty stack");
            let (outcome, _) =
                self.update_ref(repo, target, RefExpect::Version(cur.version), Some(new_head), actor, reason.into()).await?;
            if matches!(outcome, CasOutcome::Mismatch { .. }) {
                // Someone landed first: rebase again onto the new head. The
                // changesets created above are harmless unreferenced orphans.
                continue;
            }
            return Ok(LandResult { head: new_head, rebased, fast_forward: false, merged_files });
        }
        Err(FluxError::FailedPrecondition("CAS_MISMATCH: target kept moving; retry".into()))
    }

    /// The changesets in `top` not in `base`, oldest first. Must be linear.
    pub async fn stack_between(&self, repo: RepoId, top: &ChangesetId, base: &ChangesetId) -> FluxResult<Vec<Arc<Loaded>>> {
        let (g, it) = self.graph_index(repo, top).await?;
        let (_, ib) = self.graph_index(repo, base).await?;
        let ids: Vec<ChangesetId> = {
            let graph = g.graph.read().unwrap();
            graph.walk(&[it], &[ib], false).take(10_000).map(|i| graph.id(i)).collect()
        };
        let mut stack: Vec<Arc<Loaded>> = self.load_arc(repo, &ids).await?.into_iter().flatten().collect();
        stack.reverse();
        Ok(stack)
    }

    /// Rebase the stack ending at `top` onto `onto`. Returns the new ids
    /// (oldest first) and how many files needed a three-way merge.
    pub async fn rebase_stack(
        self: &Arc<Self>,
        repo: RepoId,
        top: ChangesetId,
        onto: ChangesetId,
        actor: &str,
    ) -> FluxResult<(Vec<ChangesetId>, u32)> {
        let stack = self.stack_between(repo, &top, &onto).await?;
        if stack.is_empty() {
            return Ok((vec![onto], 0));
        }
        for (i, cs) in stack.iter().enumerate() {
            if cs.changeset.parents.len() != 1 {
                return Err(FluxError::FailedPrecondition(format!(
                    "{} is a merge; only linear stacks can be rebased",
                    cs.id.short()
                )));
            }
            if i > 0 && cs.changeset.parents[0] != stack[i - 1].id {
                return Err(FluxError::FailedPrecondition("stack is not linear".into()));
            }
        }
        let mut parent = onto;
        let mut out = Vec::with_capacity(stack.len());
        let mut merged = 0u32;
        let now = now_ms();
        for cs in &stack {
            let orig_parent = cs.changeset.parents[0];
            let mut fc: BTreeMap<RepoPath, FileChange> = BTreeMap::new();
            let mut conflicts: Vec<String> = Vec::new();
            for (path, change) in &cs.changeset.file_changes {
                let base = self.entry_at(repo, &orig_parent, path).await?;
                let ours = self.entry_at(repo, &parent, path).await?;
                // A file created on our side where this change needs a directory.
                if matches!(change, FileChange::Change { .. }) {
                    let mut anc = path.parent();
                    while let Some(a) = anc {
                        let o = self.entry_at(repo, &parent, &a).await?;
                        if o.as_ref().is_some_and(|e| !e.is_dir()) && self.entry_at(repo, &orig_parent, &a).await? != o {
                            conflicts.push(format!("{path} (blocked by file {a})"));
                        }
                        anc = a.parent();
                    }
                }
                if base == ours || change_value(change) == entry_value(&ours) {
                    fc.insert(path.clone(), change.clone());
                    continue;
                }
                match self.merge3(&base, &ours, change).await? {
                    Some(resolved) => {
                        merged += 1;
                        fc.insert(path.clone(), resolved);
                    }
                    None => conflicts.push(path.to_string()),
                }
            }
            if !conflicts.is_empty() {
                conflicts.sort();
                conflicts.dedup();
                return Err(FluxError::FailedPrecondition(format!(
                    "REBASE_CONFLICT: {} conflicts with the target in: {}",
                    cs.id.short(),
                    conflicts.join(", ")
                )));
            }
            let mut extra = cs.changeset.extra.clone();
            extra.insert("flux.rebased_from".into(), cs.id.to_hex().into_bytes());
            let new = Changeset {
                parents: vec![parent],
                author: cs.changeset.author.clone(),
                author_time: cs.changeset.author_time,
                committer: Some(actor.as_bytes().to_vec()),
                committer_time: Some(Timestamp { unix_ms: now, tz_offset_min: 0 }),
                message: cs.changeset.message.clone(),
                file_changes: fc,
                extra,
            };
            let info = self.create_changeset(repo, new).await?;
            parent = ChangesetId::from_slice(&info.id).map_err(|e| FluxError::Internal(e.into()))?;
            out.push(parent);
        }
        Ok((out, merged))
    }

    /// Three-way merge of one path: `base` (original parent), `ours` (new
    /// parent) and `theirs` (the change being rebased). `None` = conflict.
    async fn merge3(
        &self,
        base: &Option<TreeEntry>,
        ours: &Option<TreeEntry>,
        theirs: &FileChange,
    ) -> FluxResult<Option<FileChange>> {
        let (Some(b), Some(o), FileChange::Change { content_id: t_id, kind: t_kind, .. }) = (base, ours, theirs) else {
            return Ok(None);
        };
        let text = |k: Option<FileKind>| matches!(k, Some(FileKind::Regular | FileKind::Executable));
        if !text(b.file_kind()) || !text(o.file_kind()) || !text(Some(*t_kind)) {
            return Ok(None);
        }
        let (Some(bid), Some(oid)) = (b.content_id(), o.content_id()) else { return Ok(None) };
        let fetch = |id: flux_core::ContentId| async move {
            self.blobs.get_content_bytes(&id).await?.ok_or_else(|| FluxError::Internal(anyhow::anyhow!("content {id} missing")))
        };
        let (bb, ob, tb) = (fetch(bid).await?, fetch(oid).await?, fetch(*t_id).await?);
        if [&bb, &ob, &tb].iter().any(|d| d.len() > (8 << 20) || d[..d.len().min(8192)].contains(&0)) {
            return Ok(None);
        }
        let merged = match diffy::merge_bytes(&bb, &ob, &tb) {
            Ok(m) => m,
            Err(_) => return Ok(None),
        };
        // Our side changed the mode only if theirs didn't.
        let kind = if b.file_kind() == Some(*t_kind) { o.file_kind().unwrap_or(*t_kind) } else { *t_kind };
        let (id, meta) = self.put_content(&merged).await?;
        Ok(Some(FileChange::Change { content_id: id, size: meta.size, kind, copy_from: None }))
    }

    // ───────────── reviews ─────────────

    pub async fn create_review(
        &self,
        repo: RepoId,
        title: &str,
        body: &str,
        author: &str,
        head: ChangesetId,
        target: &[u8],
    ) -> FluxResult<Review> {
        validate_ref_name(target)?;
        if title.trim().is_empty() {
            return Err(FluxError::InvalidArgument("a review needs a title".into()));
        }
        self.load_one_arc(repo, &head).await?;
        let r = self.meta.create_review(repo, title.trim(), body, author, head, target).await?;
        self.meta.add_review_event(repo, r.number, "opened", author, "", Some(head), None, None).await?;
        self.review_event(repo, r.number, "opened").await;
        Ok(r)
    }

    async fn review_event(&self, repo: RepoId, number: i64, kind: &str) {
        let name = self.repo_name(repo).await;
        self.publish(Event::Review { repo: name, number, kind: kind.into() });
    }

    pub async fn review(&self, repo: RepoId, number: i64) -> FluxResult<Review> {
        self.meta.get_review(repo, number).await?.ok_or_else(|| FluxError::NotFound(format!("review #{number} not found")))
    }

    pub async fn review_detail(&self, repo: RepoId, number: i64) -> FluxResult<ReviewDetail> {
        let review = self.review(repo, number).await?;
        let events = self.meta.review_events(repo, number).await?;
        let target_head = self.meta.get_ref(repo, &review.target).await?.map(|r| r.target);
        let stack = match (review.state.as_str(), review.landed, target_head) {
            // Landed reviews show what landed (rebased changesets).
            ("landed", Some(landed), _) => {
                let mut s = Vec::new();
                for e in &events {
                    if e.kind == "landed"
                        && let Some(base) = e.cs
                    {
                        s = self.stack_between(repo, &landed, &base).await.unwrap_or_default();
                    }
                }
                s
            }
            (_, _, Some(h)) => self.stack_between(repo, &review.head, &h).await?,
            _ => self.stack_between_root(repo, &review.head).await?,
        };
        let mut stack = stack;
        stack.reverse();
        Ok(ReviewDetail { review, events, stack, target_head })
    }

    async fn stack_between_root(&self, repo: RepoId, top: &ChangesetId) -> FluxResult<Vec<Arc<Loaded>>> {
        let (g, it) = self.graph_index(repo, top).await?;
        let ids: Vec<ChangesetId> = {
            let graph = g.graph.read().unwrap();
            graph.walk(&[it], &[], true).take(100).map(|i| graph.id(i)).collect()
        };
        let mut v: Vec<Arc<Loaded>> = self.load_arc(repo, &ids).await?.into_iter().flatten().collect();
        v.reverse();
        Ok(v)
    }

    /// `action`: comment | approve | close | reopen | update.
    pub async fn update_review(
        &self,
        repo: RepoId,
        number: i64,
        action: &str,
        actor: &str,
        body: &str,
        head: Option<ChangesetId>,
    ) -> FluxResult<Review> {
        let r = self.review(repo, number).await?;
        let update = match action {
            "comment" => {
                if body.trim().is_empty() {
                    return Err(FluxError::InvalidArgument("empty comment".into()));
                }
                ReviewUpdate::default()
            }
            "approve" => ReviewUpdate::default(),
            "close" if r.state == "open" => ReviewUpdate { state: Some("closed".into()), ..Default::default() },
            "reopen" if r.state == "closed" => ReviewUpdate { state: Some("open".into()), ..Default::default() },
            "update" if r.state == "open" => {
                let h = head.ok_or_else(|| FluxError::InvalidArgument("update needs a new head".into()))?;
                self.load_one_arc(repo, &h).await?;
                ReviewUpdate { head: Some(h), ..Default::default() }
            }
            "close" | "reopen" | "update" => {
                return Err(FluxError::FailedPrecondition(format!("cannot {action} a review that is {}", r.state)));
            }
            other => return Err(FluxError::InvalidArgument(format!("unknown review action {other:?}"))),
        };
        let kind = match action {
            "comment" => "comment",
            "approve" => "approved",
            "close" => "closed",
            "reopen" => "reopened",
            _ => "updated",
        };
        let r =
            self.meta.update_review(repo, number, update).await?.ok_or_else(|| FluxError::NotFound("review vanished".into()))?;
        self.meta.add_review_event(repo, number, kind, actor, body, head, None, None).await?;
        self.review_event(repo, number, kind).await;
        Ok(r)
    }

    /// A comment anchored to a line of the review's diff. `line > 0` is a
    /// new-side line number, `line < 0` an old-side one.
    pub async fn comment_review_at(
        &self,
        repo: RepoId,
        number: i64,
        actor: &str,
        body: &str,
        path: &[u8],
        line: i64,
    ) -> FluxResult<()> {
        let r = self.review(repo, number).await?;
        if body.trim().is_empty() {
            return Err(FluxError::InvalidArgument("empty comment".into()));
        }
        if line == 0 || path.is_empty() {
            return Err(FluxError::InvalidArgument("a line comment needs a path and a line".into()));
        }
        self.meta.add_review_event(repo, number, "comment", actor, body, Some(r.head), Some(path), Some(line)).await?;
        self.meta.update_review(repo, number, ReviewUpdate::default()).await?;
        self.review_event(repo, number, "comment").await;
        Ok(())
    }

    pub async fn land_review(self: &Arc<Self>, repo: RepoId, number: i64, actor: &str) -> FluxResult<LandResult> {
        let r = self.review(repo, number).await?;
        if r.state != "open" {
            return Err(FluxError::FailedPrecondition(format!("review #{number} is {}", r.state)));
        }
        let base = self.meta.get_ref(repo, &r.target).await?.map(|x| x.target);
        let result = self.land(repo, r.head, &r.target, actor, &format!("land review #{number}")).await?;
        self.meta
            .update_review(
                repo,
                number,
                ReviewUpdate { state: Some("landed".into()), landed: Some(result.head), ..Default::default() },
            )
            .await?;
        let note = if result.fast_forward {
            "fast-forward".to_string()
        } else {
            format!("rebased {} changeset(s), {} file(s) merged", result.rebased.len(), result.merged_files)
        };
        self.meta.add_review_event(repo, number, "landed", actor, &note, base, None, None).await?;
        self.review_event(repo, number, "landed").await;
        Ok(result)
    }
}
