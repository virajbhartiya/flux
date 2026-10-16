//! git → flux history import (SPEC §10.1).
//!
//! Commits are processed parents-first. Each becomes a changeset whose file
//! changes are the diff against the first parent; merges additionally list
//! every path where any other parent differs from the first parent (SPEC §3.3
//! merge rule). Import is incremental and resumable: commits already mapped
//! on the server are skipped.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use flux_client::Client;
use flux_core::{Changeset, ChangesetId, ContentId, FileChange, FileKind, RepoPath, Timestamp};
use flux_proto::v1::update_ref_request::Expect;

use crate::git::{self, CatFile, Commit, DiffRequest, DiffTree, RawEntry, Sha};

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub git_dir: PathBuf,
    /// `for-each-ref` patterns; default `refs/heads/`.
    pub ref_patterns: Vec<String>,
    /// Commits per create batch.
    pub batch: usize,
    /// Flush uploads after this many bytes.
    pub upload_bytes: usize,
    /// Update flux refs for imported branches.
    pub set_refs: bool,
}

impl ImportOptions {
    pub fn new(git_dir: impl Into<PathBuf>) -> Self {
        Self { git_dir: git_dir.into(), ref_patterns: vec![], batch: 512, upload_bytes: 32 << 20, set_refs: true }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ImportStats {
    pub commits_seen: usize,
    pub commits_imported: usize,
    pub commits_skipped: usize,
    pub blobs_uploaded: usize,
    pub blob_bytes: u64,
    pub refs_updated: usize,
    pub elapsed_ms: u128,
}

/// Map a git mode to a flux file kind. Legacy modes like 100664 are kept
/// (`FileKind::GitMode`) so regenerated trees hash to the original SHA-1s.
pub fn mode_kind(mode: u32, sha: &Sha) -> Result<Option<FileKind>> {
    Ok(match mode & 0o170000 {
        0 if mode == 0 => None,
        0o100000 if mode & 0o7000 == 0 => Some(FileKind::from_git_file_mode(mode)),
        0o120000 => Some(FileKind::Symlink),
        0o160000 => Some(FileKind::GitLink(*sha)),
        _ => bail!("unsupported git mode {mode:o}"),
    })
}

/// A file value on one side of a diff: `None` = absent.
type Side = Option<(FileKind, Sha)>;

/// A path's value in an imported commit.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Set(Side),
    /// Merge resolution equals the first parent (FileChange::Retain).
    Retain,
}

struct Pending {
    sha: Sha,
    parents: Vec<Sha>,
    commit: Commit,
    /// path → value in this commit (git shas; contents resolved later).
    changes: BTreeMap<Vec<u8>, Value>,
}

fn side(mode: u32, sha: &Sha) -> Result<Side> {
    Ok(mode_kind(mode, sha)?.map(|k| (k, *sha)))
}

/// Compute the file-change set (git shas) for a commit from its diff records.
fn file_changes(first: &[RawEntry], others: &[Vec<RawEntry>]) -> Result<BTreeMap<Vec<u8>, Value>> {
    let mut out = BTreeMap::new();
    for e in first {
        out.insert(e.path.clone(), Value::Set(side(e.new_mode, &e.new_sha)?));
    }
    // Paths where another parent differs from the first parent, but this
    // commit equals the first parent: an explicit "keep parents[0]" (or a
    // deletion when parents[0] has nothing there).
    for rec in others {
        for e in rec {
            out.entry(e.path.clone()).or_insert(if e.old_mode == 0 { Value::Set(None) } else { Value::Retain });
        }
    }
    Ok(out)
}

/// git's diff output canonicalizes legacy modes, so restore them: files
/// with a legacy mode in this commit get `FileKind::GitMode`, and files
/// whose legacy mode differs from the first parent's (including mode-only
/// changes, which git's diff does not report) get an explicit change.
fn apply_legacy_modes(changes: &mut BTreeMap<Vec<u8>, Value>, cur: &git::LegacyMap, prev: &git::LegacyMap) {
    for (path, (mode, sha)) in cur {
        match changes.get_mut(path) {
            Some(Value::Set(Some((kind, _)))) => *kind = FileKind::GitMode(*mode),
            Some(_) => {}
            None if prev.get(path) != Some(&(*mode, *sha)) => {
                changes.insert(path.clone(), Value::Set(Some((FileKind::GitMode(*mode), *sha))));
            }
            None => {}
        }
    }
    for (path, (mode, sha)) in prev {
        if !cur.contains_key(path) && !changes.contains_key(path) {
            // Unchanged content, now with the canonical mode.
            changes.insert(path.clone(), Value::Set(Some((FileKind::GitMode(*mode).normalized(), *sha))));
        }
    }
}

fn ts(t: (i64, i16)) -> Timestamp {
    Timestamp { unix_ms: t.0.saturating_mul(1000), tz_offset_min: t.1 }
}

pub async fn import(client: &Client, opts: &ImportOptions) -> Result<ImportStats> {
    let started = Instant::now();
    let repo = opts.git_dir.as_path();
    let mut stats = ImportStats::default();

    let refs = git::list_refs(repo, &opts.ref_patterns)?;
    if refs.is_empty() {
        bail!("no refs matched {:?} in {}", opts.ref_patterns, repo.display());
    }
    let tips: Vec<Sha> = refs.iter().map(|r| r.1).collect();
    let commits = git::rev_list(repo, &tips)?;
    stats.commits_seen = commits.len();
    tracing::info!(commits = commits.len(), refs = refs.len(), "scanned git history");

    // Resume: find already-imported commits.
    let shas: Vec<Sha> = commits.iter().map(|c| c.0).collect();
    let mut mapped: HashMap<Sha, ChangesetId> = HashMap::new();
    for (sha, cs) in shas.iter().zip(client.git_lookup(&shas).await?) {
        if let Some(cs) = cs {
            mapped.insert(*sha, cs);
        }
    }
    stats.commits_skipped = mapped.len();
    let todo: Vec<(Sha, Vec<Sha>)> = commits.into_iter().filter(|c| !mapped.contains_key(&c.0)).collect();

    if !todo.is_empty() {
        import_commits(client, repo, opts, todo, &mut mapped, &mut stats).await?;
    }

    if opts.set_refs {
        for (name, sha) in &refs {
            // refs/heads/x → x, refs/tags/x → tags/x, refs/pull/1/head → pull/1/head
            // (the names the git frontend serves back under refs/).
            let flux_name = name
                .strip_prefix("refs/heads/")
                .map(str::to_string)
                .or_else(|| name.strip_prefix("refs/").map(str::to_string))
                .unwrap_or_else(|| name.clone());
            let cs = mapped.get(sha).ok_or_else(|| anyhow!("ref {name} points at unimported commit"))?;
            client.update_ref(&flux_name, Expect::Any(true), Some(*cs), "git import").await?;
            stats.refs_updated += 1;
        }
    }
    stats.elapsed_ms = started.elapsed().as_millis();
    Ok(stats)
}

/// One batch produced by the git reader thread: everything needed to create
/// `changesets` once `uploads` are stored.
struct ReadBatch {
    shas: Vec<Sha>,
    changesets: Vec<Changeset>,
    /// `(id, data, delta base)`: new blobs, with the previous version of the
    /// same path as a delta hint when known.
    uploads: Vec<(ContentId, Bytes, Option<ContentId>)>,
}

/// Read commits, diffs and blobs from git on a dedicated thread. Changeset
/// ids are computed locally, so later commits can name earlier ones without
/// waiting for the server; the async side only uploads and creates.
fn read_batches(
    repo: PathBuf,
    batch: usize,
    todo: Vec<(Sha, Vec<Sha>)>,
    mut mapped: HashMap<Sha, ChangesetId>,
    tx: tokio::sync::mpsc::Sender<Result<ReadBatch>>,
) -> Result<()> {
    // One diff request per (commit, parent) pair, in commit order.
    let mut requests = Vec::new();
    for (sha, parents) in &todo {
        match parents.first() {
            None => requests.push(DiffRequest { commit: *sha, parent: None }),
            Some(p0) => {
                requests.push(DiffRequest { commit: *sha, parent: Some(*p0) });
                for pi in &parents[1..] {
                    requests.push(DiffRequest { commit: *pi, parent: Some(*p0) });
                }
            }
        }
    }
    let mut diffs = DiffTree::spawn(&repo, requests)?;
    let mut cat = CatFile::new(&repo)?;
    let mut pool = git::CatPool::new(&repo, 4)?;
    let mut legacy = git::LegacyModes::scan(&repo)?;
    // Root tree per commit, for comparing legacy modes with the first parent.
    let mut trees: HashMap<Sha, Sha> = HashMap::new();
    let mut blobs: HashMap<Sha, (ContentId, u64)> = HashMap::new();
    for chunk in todo.chunks(batch.max(1)) {
        let mut out = ReadBatch { shas: vec![], changesets: vec![], uploads: vec![] };
        // Pass 1: commits and diffs, collecting the blobs this batch adds.
        let commit_shas: Vec<Sha> = chunk.iter().map(|c| c.0).collect();
        let mut staged = Vec::with_capacity(chunk.len());
        let mut wanted: Vec<Sha> = Vec::new();
        let mut wanted_set: std::collections::HashSet<Sha> = Default::default();
        for ((sha, parents), (ty, data)) in chunk.iter().zip(cat.read_many(&commit_shas)?) {
            if ty != "commit" {
                bail!("{} is a {ty}, expected commit", hex::encode(sha));
            }
            let commit = git::parse_commit(&data)?;
            let first = diffs.next_record()?;
            let mut others = Vec::new();
            for _ in 1..parents.len() {
                others.push(diffs.next_record()?);
            }
            let mut changes = file_changes(&first, &others).with_context(|| format!("commit {}", hex::encode(sha)))?;
            if !legacy.is_empty() {
                let parent_tree = match parents.first() {
                    None => None,
                    Some(p0) => Some(match trees.get(p0) {
                        Some(t) => *t,
                        None => git::parse_commit(&cat.read(p0)?.1)?.tree,
                    }),
                };
                let cur = legacy.files(&mut cat, &commit.tree)?;
                let prev = match parent_tree {
                    Some(t) => legacy.files(&mut cat, &t)?,
                    None => Default::default(),
                };
                apply_legacy_modes(&mut changes, &cur, &prev);
            }
            trees.insert(*sha, commit.tree);
            for value in changes.values() {
                if let Value::Set(Some((kind, bsha))) = value
                    && !matches!(kind, FileKind::GitLink(_))
                    && !blobs.contains_key(bsha)
                    && wanted_set.insert(*bsha)
                {
                    wanted.push(*bsha);
                }
            }
            staged.push((*sha, parents, commit, changes, first));
        }
        // Pass 2: read and hash the new blobs in parallel.
        let fetched = pool.read_map(&wanted, |bsha, ty, data| {
            if ty != "blob" {
                bail!("{} is a {ty}, expected blob", hex::encode(bsha));
            }
            Ok((ContentId::hash(&data), Bytes::from(data)))
        })?;
        let mut data: HashMap<Sha, Bytes> = HashMap::with_capacity(wanted.len());
        for (bsha, (id, bytes)) in wanted.iter().zip(fetched) {
            blobs.insert(*bsha, (id, bytes.len() as u64));
            data.insert(*bsha, bytes);
        }
        // Pass 3: changesets and uploads in commit order (bases first).
        for (sha, parents, commit, changes, first) in staged {
            // Previous version of each modified path, for delta hints.
            let old: HashMap<&[u8], Sha> =
                first.iter().filter(|e| e.old_mode & 0o170000 == 0o100000).map(|e| (e.path.as_slice(), e.old_sha)).collect();
            for (path, value) in &changes {
                let Value::Set(Some((kind, bsha))) = value else { continue };
                if matches!(kind, FileKind::GitLink(_)) {
                    continue;
                }
                let Some(bytes) = data.remove(bsha) else { continue };
                let base = old.get(path.as_slice()).and_then(|o| blobs.get(o)).map(|b| b.0);
                out.uploads.push((blobs[bsha].0, bytes, base));
            }
            let p = Pending { sha, parents: parents.clone(), commit, changes };
            let cs = to_changeset(&p, &mapped, &blobs).with_context(|| format!("converting commit {}", hex::encode(sha)))?;
            mapped.insert(sha, cs.id());
            out.shas.push(sha);
            out.changesets.push(cs);
        }
        if tx.blocking_send(Ok(out)).is_err() {
            return Ok(()); // consumer failed; it reports its own error
        }
    }
    diffs.finish()
}

async fn import_commits(
    client: &Client,
    repo: &Path,
    opts: &ImportOptions,
    todo: Vec<(Sha, Vec<Sha>)>,
    mapped: &mut HashMap<Sha, ChangesetId>,
    stats: &mut ImportStats,
) -> Result<()> {
    let total = todo.len();
    let (read_tx, mut read_rx) = tokio::sync::mpsc::channel::<Result<ReadBatch>>(3);
    let reader = {
        let (repo, batch, mapped) = (repo.to_path_buf(), opts.batch, mapped.clone());
        std::thread::spawn(move || {
            let err_tx = read_tx.clone();
            if let Err(e) = read_batches(repo, batch, todo, mapped, read_tx) {
                let _ = err_tx.blocking_send(Err(e));
            }
        })
    };

    // Stage 2 (this task): upload. Stage 3 (spawned): create + map. Uploads
    // for batch k+1 overlap with creation of batch k.
    let (create_tx, mut create_rx) = tokio::sync::mpsc::channel::<ReadBatch>(2);
    let creator = {
        let client = client.clone();
        tokio::spawn(async move {
            // Batches are pipelined: the server prepares batch k+1 while k
            // commits. Responses are read concurrently, in order.
            let (sender, mut receiver) = client.create_stream().await?;
            let feeder = tokio::spawn(async move {
                let (mut shas, mut idle) = (std::collections::VecDeque::new(), std::time::Duration::ZERO);
                let mut waited = Instant::now();
                while let Some(b) = create_rx.recv().await {
                    idle += waited.elapsed();
                    if sender.send(&b.changesets, &b.shas).await.is_err() {
                        break; // the receiver reports the server's error
                    }
                    shas.push_back(b.shas);
                    waited = Instant::now();
                }
                tracing::debug!(idle_ms = idle.as_millis() as u64, "stage create");
                shas
            });
            let received = async {
                let mut ids: Vec<ChangesetId> = Vec::new();
                let (mut last_log, mut since) = (Instant::now(), 0usize);
                while let Some(batch) = receiver.next().await.context("creating changesets")? {
                    since += batch.len();
                    ids.extend(batch);
                    if last_log.elapsed().as_secs() >= 5 {
                        let rate = since as f64 / last_log.elapsed().as_secs_f64();
                        tracing::info!(done = ids.len(), total, "importing ({rate:.0} commits/s recent)");
                        last_log = Instant::now();
                        since = 0;
                    }
                }
                anyhow::Ok(ids)
            }
            .await;
            let ids = match received {
                Ok(ids) => ids,
                Err(e) => {
                    feeder.abort();
                    return Err(e);
                }
            };
            let shas: Vec<Sha> = feeder.await?.into_iter().flatten().collect();
            if shas.len() != ids.len() {
                bail!("created {} of {} changesets", ids.len(), shas.len());
            }
            let created: Vec<(Sha, ChangesetId)> = shas.into_iter().zip(ids).collect();
            anyhow::Ok(created)
        })
    };

    let mut upload_err = None;
    let (mut up_busy, mut up_wait_read, mut up_wait_create) =
        (std::time::Duration::ZERO, std::time::Duration::ZERO, std::time::Duration::ZERO);
    let mut t = Instant::now();
    while let Some(batch) = read_rx.recv().await {
        up_wait_read += t.elapsed();
        let batch = match batch {
            Ok(b) => b,
            Err(e) => {
                upload_err = Some(e);
                break;
            }
        };
        stats.blobs_uploaded += batch.uploads.len();
        stats.blob_bytes += batch.uploads.iter().map(|u| u.1.len() as u64).sum::<u64>();
        let uploads = batch.uploads.clone();
        let tu = Instant::now();
        if let Err(e) = client.upload_known(uploads).await {
            upload_err = Some(e);
            break;
        }
        up_busy += tu.elapsed();
        let tc = Instant::now();
        if create_tx.send(ReadBatch { uploads: vec![], ..batch }).await.is_err() {
            break; // creator failed
        }
        up_wait_create += tc.elapsed();
        t = Instant::now();
    }
    tracing::debug!(
        busy_ms = up_busy.as_millis() as u64,
        waiting_for_git_ms = up_wait_read.as_millis() as u64,
        waiting_for_create_ms = up_wait_create.as_millis() as u64,
        "stage upload"
    );
    drop(create_tx);
    drop(read_rx);
    let created = creator.await?;
    let _ = reader.join();
    let created = match (created, upload_err) {
        (Err(e), _) | (_, Some(e)) => return Err(e),
        (Ok(c), None) => c,
    };
    stats.commits_imported += created.len();
    mapped.extend(created);
    Ok(())
}

fn to_changeset(p: &Pending, mapped: &HashMap<Sha, ChangesetId>, blobs: &HashMap<Sha, (ContentId, u64)>) -> Result<Changeset> {
    let parents = p
        .parents
        .iter()
        .map(|s| mapped.get(s).copied().ok_or_else(|| anyhow!("parent {} not imported", hex::encode(s))))
        .collect::<Result<Vec<_>>>()?;
    let mut file_changes = BTreeMap::new();
    for (path, value) in &p.changes {
        let rp =
            RepoPath::new(path.clone()).with_context(|| format!("commit {} has a path flux cannot store", hex::encode(p.sha)))?;
        let change = match value {
            Value::Retain => FileChange::Retain,
            Value::Set(None) => FileChange::Deletion,
            Value::Set(Some((kind @ FileKind::GitLink(_), _))) => {
                FileChange::Change { content_id: ContentId::empty(), size: 0, kind: *kind, copy_from: None }
            }
            Value::Set(Some((kind, sha))) => {
                let (content_id, size) = blobs[sha];
                FileChange::Change { content_id, size, kind: *kind, copy_from: None }
            }
        };
        file_changes.insert(rp, change);
    }
    let c = &p.commit;
    let mut extra = BTreeMap::new();
    if !c.extra_headers.is_empty() {
        extra.insert("git.extra_headers".to_string(), c.extra_headers.clone());
    }
    if let Some((a, cm)) = &c.raw_identity {
        extra.insert("git.raw_author".to_string(), a.clone());
        extra.insert("git.raw_committer".to_string(), cm.clone());
    }
    Ok(Changeset {
        parents,
        author: c.author.clone(),
        author_time: ts(c.author_time),
        committer: Some(c.committer.clone()),
        committer_time: Some(ts(c.committer_time)),
        message: c.message.clone(),
        file_changes,
        extra,
    })
}

/// Result of verifying one commit.
#[derive(Debug)]
pub struct VerifyMismatch {
    pub commit: String,
    pub detail: String,
}

/// Compare flux's file set at each commit against `git ls-tree -r` (the
/// oracle check from SPEC §17 phase 1). Returns mismatches (empty = pass).
pub async fn verify(client: &Client, git_dir: &Path, commits: &[Sha]) -> Result<Vec<VerifyMismatch>> {
    let mut cat = CatFile::new(git_dir)?;
    let mut blob_ids: HashMap<Sha, ContentId> = HashMap::new();
    let mut mismatches = Vec::new();
    for sha in commits {
        let want = {
            let mut m: BTreeMap<Vec<u8>, (FileKind, Option<ContentId>)> = BTreeMap::new();
            for (mode, bsha, path) in git::ls_tree(git_dir, sha)? {
                let kind = mode_kind(mode, &bsha)?.ok_or_else(|| anyhow!("zero mode in ls-tree"))?;
                let cid = match kind {
                    FileKind::GitLink(_) => None,
                    _ => Some(match blob_ids.get(&bsha) {
                        Some(c) => *c,
                        None => {
                            let (_, data) = cat.read(&bsha)?;
                            let c = ContentId::hash(&data);
                            blob_ids.insert(bsha, c);
                            c
                        }
                    }),
                };
                m.insert(path, (kind, cid));
            }
            m
        };
        let info = client.resolve(&format!("git:{}", hex::encode(sha))).await?;
        let mut got: BTreeMap<Vec<u8>, (FileKind, Option<ContentId>)> = BTreeMap::new();
        for (_, path, tree) in client.get_trees(&[info.root_tree], 4096, &[], b"").await? {
            for e in tree.entries() {
                if let Some(kind) = e.file_kind() {
                    let mut full = path.clone();
                    if !full.is_empty() {
                        full.push(b'/');
                    }
                    full.extend_from_slice(e.name.as_bytes());
                    // git's ls-tree reports canonical modes.
                    got.insert(full, (kind.normalized(), e.content_id()));
                }
            }
        }
        if got != want {
            let missing: Vec<_> =
                want.keys().filter(|k| !got.contains_key(*k)).take(3).map(|k| String::from_utf8_lossy(k).into_owned()).collect();
            let extra: Vec<_> =
                got.keys().filter(|k| !want.contains_key(*k)).take(3).map(|k| String::from_utf8_lossy(k).into_owned()).collect();
            let differ: Vec<_> = want
                .iter()
                .filter(|(k, v)| got.get(*k).is_some_and(|g| g != *v))
                .take(3)
                .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
                .collect();
            mismatches.push(VerifyMismatch {
                commit: hex::encode(sha),
                detail: format!("missing {missing:?}, unexpected {extra:?}, differing {differ:?}"),
            });
        }
    }
    Ok(mismatches)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_modes_restore_kinds_and_mode_only_changes() {
        let (a, b, c) = ([1u8; 20], [2u8; 20], [3u8; 20]);
        let prev: git::LegacyMap = [(b"kept".to_vec(), (0o100664, a)), (b"fixed".to_vec(), (0o100664, b))].into();
        let cur: git::LegacyMap = [(b"kept".to_vec(), (0o100664, a)), (b"new".to_vec(), (0o100664, c))].into();
        // git's diff reports the new file with a canonical mode and nothing for
        // `fixed` (100664 → 100644 is not a change to git's diff).
        let mut changes: BTreeMap<Vec<u8>, Value> = [(b"new".to_vec(), Value::Set(Some((FileKind::Regular, c))))].into();
        apply_legacy_modes(&mut changes, &cur, &prev);
        assert_eq!(changes[b"new".as_slice()], Value::Set(Some((FileKind::GitMode(0o100664), c))));
        assert_eq!(changes[b"fixed".as_slice()], Value::Set(Some((FileKind::Regular, b))));
        assert!(!changes.contains_key(b"kept".as_slice()), "unchanged legacy files need no entry");
    }

    #[test]
    fn raw_tree_entries() {
        let mut t = b"100664 a.txt\0".to_vec();
        t.extend([7u8; 20]);
        t.extend(b"40000 dir\0");
        t.extend([8u8; 20]);
        let e = git::tree_entries(&t).unwrap();
        assert_eq!(
            e,
            vec![(b"100664".as_slice(), b"a.txt".as_slice(), [7; 20]), (b"40000".as_slice(), b"dir".as_slice(), [8; 20])]
        );
    }
}
