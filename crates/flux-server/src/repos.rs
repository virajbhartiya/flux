//! Repository lifecycle and distribution: releases (with assets), forks
//! (and reviews from forks), archives, renames, deletion.

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::sync::Arc;

use flux_core::{ChangesetId, FileKind};
use flux_store::{NewChangeset, RefExpect, Release, ReleaseAsset, RepoId};
use serde::{Deserialize, Serialize};

use crate::auth::{Principal, now_ms};
use crate::core::{Event, Flux, FluxError, FluxResult};
use crate::rbac::Role;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReleaseInput {
    pub tag: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub draft: Option<bool>,
    #[serde(default)]
    pub prerelease: Option<bool>,
    /// Where to create the tag if it does not exist (default branch).
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseJson {
    pub id: i64,
    pub tag: String,
    pub name: String,
    pub body: String,
    pub draft: bool,
    pub prerelease: bool,
    pub author: String,
    pub target: Option<String>,
    pub created_at: i64,
    pub published_at: Option<i64>,
    pub assets: Vec<AssetJson>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssetJson {
    pub id: i64,
    pub name: String,
    pub size: i64,
    pub content_type: String,
    pub downloads: i64,
    pub url: String,
}

/// Archive formats for downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    TarGz,
    Zip,
}

fn valid_asset_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 200 && !n.contains(['/', '\\', '\0']) && n != "." && n != ".."
}

impl Flux {
    pub async fn release_json(&self, repo: RepoId, r: &Release) -> FluxResult<ReleaseJson> {
        let name = self.repo_name(repo).await;
        let assets = self.meta.release_assets(r.id).await?;
        Ok(ReleaseJson {
            id: r.id,
            tag: r.tag.clone(),
            name: r.name.clone(),
            body: r.body.clone(),
            draft: r.draft,
            prerelease: r.prerelease,
            author: r.author.clone(),
            target: r.target.map(|t| t.to_hex()),
            created_at: r.created_ms,
            published_at: r.published_ms,
            assets: assets
                .iter()
                .map(|a| AssetJson {
                    id: a.id,
                    name: a.name.clone(),
                    size: a.size,
                    content_type: a.content_type.clone(),
                    downloads: a.downloads,
                    url: format!("/{name}/-/releases/download/{}/{}", r.tag, a.name),
                })
                .collect(),
        })
    }

    /// Releases visible to the principal (drafts need write access).
    pub async fn list_releases_as(&self, who: &Principal, repo: RepoId) -> FluxResult<Vec<ReleaseJson>> {
        let role = self.authorize(who, repo, Role::Read).await?;
        let mut out = Vec::new();
        for r in self.meta.list_releases(repo, 200).await? {
            if r.draft && role < Role::Write {
                continue;
            }
            out.push(self.release_json(repo, &r).await?);
        }
        Ok(out)
    }

    pub async fn release_by_tag_as(&self, who: &Principal, repo: RepoId, tag: &str) -> FluxResult<Release> {
        let role = self.authorize(who, repo, Role::Read).await?;
        self.meta
            .release_by_tag(repo, tag)
            .await?
            .filter(|r| !r.draft || role >= Role::Write)
            .ok_or_else(|| FluxError::NotFound(format!("no release for {tag}")))
    }

    pub async fn create_release(self: &Arc<Self>, who: &Principal, repo: RepoId, input: ReleaseInput) -> FluxResult<ReleaseJson> {
        self.authorize(who, repo, Role::Write).await?;
        let tag = input.tag.trim().trim_start_matches("refs/tags/").to_string();
        let tag_ref = format!("tags/{tag}");
        crate::core::validate_ref_name(tag_ref.as_bytes())?;
        let target = match self.meta.get_ref(repo, tag_ref.as_bytes()).await? {
            Some(r) => r.target,
            None => {
                let rev = match input.target.as_deref().filter(|t| !t.is_empty()) {
                    Some(t) => t.to_string(),
                    None => self.repo_config(repo).await?.default_branch.clone().unwrap_or_else(|| "main".into()),
                };
                let cs = self.resolve_rev(repo, &rev).await?;
                if !input.draft.unwrap_or(false) {
                    self.update_ref_as(who, repo, tag_ref.as_bytes(), RefExpect::Absent, Some(cs), format!("release {tag}"))
                        .await?;
                }
                cs
            }
        };
        let now = now_ms();
        let draft = input.draft.unwrap_or(false);
        let r = self
            .meta
            .create_release(
                repo,
                &Release {
                    id: 0,
                    tag: tag.clone(),
                    name: input.name.unwrap_or_else(|| tag.clone()),
                    body: input.body.unwrap_or_default(),
                    draft,
                    prerelease: input.prerelease.unwrap_or(false),
                    author: who.name().into(),
                    target: Some(target),
                    created_ms: now,
                    published_ms: (!draft).then_some(now),
                },
            )
            .await?;
        let name = self.repo_name(repo).await;
        self.publish(Event::Release {
            repo: name,
            id: r.id,
            action: if draft { "created" } else { "published" }.into(),
            actor: who.name().into(),
        });
        self.release_json(repo, &r).await
    }

    pub async fn update_release(
        self: &Arc<Self>,
        who: &Principal,
        repo: RepoId,
        id: i64,
        input: ReleaseInput,
    ) -> FluxResult<ReleaseJson> {
        self.authorize(who, repo, Role::Write).await?;
        let mut r = self.meta.get_release(repo, id).await?.ok_or_else(|| FluxError::NotFound("release not found".into()))?;
        let was_draft = r.draft;
        if let Some(n) = input.name {
            r.name = n;
        }
        if let Some(b) = input.body {
            r.body = b;
        }
        if let Some(p) = input.prerelease {
            r.prerelease = p;
        }
        if let Some(d) = input.draft {
            r.draft = d;
        }
        if was_draft && !r.draft {
            // Publishing creates the tag.
            let tag_ref = format!("tags/{}", r.tag);
            if self.meta.get_ref(repo, tag_ref.as_bytes()).await?.is_none()
                && let Some(t) = r.target
            {
                self.update_ref_as(who, repo, tag_ref.as_bytes(), RefExpect::Absent, Some(t), format!("release {}", r.tag))
                    .await?;
            }
            r.published_ms = Some(now_ms());
        }
        self.meta.update_release(repo, &r).await?;
        let name = self.repo_name(repo).await;
        let action = if was_draft && !r.draft { "published" } else { "edited" };
        self.publish(Event::Release { repo: name, id, action: action.into(), actor: who.name().into() });
        self.release_json(repo, &r).await
    }

    pub async fn delete_release(&self, who: &Principal, repo: RepoId, id: i64) -> FluxResult<()> {
        self.authorize(who, repo, Role::Write).await?;
        if !self.meta.delete_release(repo, id).await? {
            return Err(FluxError::NotFound("release not found".into()));
        }
        let name = self.repo_name(repo).await;
        self.publish(Event::Release { repo: name, id, action: "deleted".into(), actor: who.name().into() });
        Ok(())
    }

    pub async fn upload_asset(
        &self,
        who: &Principal,
        repo: RepoId,
        release: i64,
        name: &str,
        content_type: &str,
        data: bytes::Bytes,
    ) -> FluxResult<AssetJson> {
        self.authorize(who, repo, Role::Write).await?;
        if !valid_asset_name(name) {
            return Err(FluxError::InvalidArgument("invalid asset name".into()));
        }
        let r = self.meta.get_release(repo, release).await?.ok_or_else(|| FluxError::NotFound("release not found".into()))?;
        let size = data.len() as i64;
        let (id, _) = self.put_contents(vec![(data, None)]).await?.remove(0);
        let a = self
            .meta
            .add_release_asset(&ReleaseAsset {
                id: 0,
                release_id: r.id,
                name: name.into(),
                content: id,
                size,
                content_type: if content_type.is_empty() { "application/octet-stream".into() } else { content_type.into() },
                downloads: 0,
                created_ms: now_ms(),
            })
            .await?;
        let repo_name = self.repo_name(repo).await;
        Ok(AssetJson {
            id: a.id,
            name: a.name.clone(),
            size,
            content_type: a.content_type,
            downloads: 0,
            url: format!("/{repo_name}/-/releases/download/{}/{}", r.tag, a.name),
        })
    }

    /// An asset's bytes (counts the download).
    pub async fn download_asset(
        &self,
        who: &Principal,
        repo: RepoId,
        tag: &str,
        name: &str,
    ) -> FluxResult<(ReleaseAsset, bytes::Bytes)> {
        let r = self.release_by_tag_as(who, repo, tag).await?;
        let a = self
            .meta
            .release_assets(r.id)
            .await?
            .into_iter()
            .find(|a| a.name == name)
            .ok_or_else(|| FluxError::NotFound(format!("no asset {name}")))?;
        let data = self
            .blobs
            .get_content_bytes(&a.content)
            .await?
            .ok_or_else(|| FluxError::Internal(anyhow::anyhow!("asset content missing")))?;
        let _ = self.meta.count_asset_download(a.id).await;
        Ok((a, data))
    }

    pub async fn delete_asset(&self, who: &Principal, repo: RepoId, release: i64, asset: i64) -> FluxResult<()> {
        self.authorize(who, repo, Role::Write).await?;
        self.meta.get_release(repo, release).await?.ok_or_else(|| FluxError::NotFound("release not found".into()))?;
        if !self.meta.delete_release_asset(release, asset).await? {
            return Err(FluxError::NotFound("asset not found".into()));
        }
        Ok(())
    }

    // ───────────── forks ─────────────

    /// Copy changesets (with root trees and git mappings) from one
    /// repository into another. Content is shared, so this copies only
    /// metadata. Ids already present are skipped.
    async fn copy_changesets(&self, from: RepoId, to: RepoId, ids: &[ChangesetId]) -> FluxResult<usize> {
        let mut copied = 0;
        for chunk in ids.chunks(2000) {
            let stored = self.meta.get_changesets(from, chunk).await?;
            let trees = self.meta.get_derived(from, flux_derive::TREE_V1, chunk).await?;
            let shas = self.meta.cs_to_git(from, chunk).await?;
            let mut batch = Vec::with_capacity(chunk.len());
            for (i, s) in stored.into_iter().enumerate() {
                let Some(s) = s else { continue };
                let loaded = self.load_one_arc(from, &s.id).await?;
                batch.push(NewChangeset {
                    id: s.id,
                    parents: loaded.changeset.parents.clone(),
                    gen_num: s.gen_num,
                    body: s.body,
                    derived: trees[i].clone().map(|t| vec![(flux_derive::TREE_V1, t)]).unwrap_or_default(),
                    git_sha1: shas[i],
                });
            }
            copied += batch.len();
            self.meta.put_changesets(to, &batch).await?;
        }
        Ok(copied)
    }

    /// Fork `src` to `dest` (`owner/name`): history, branches and tags.
    pub async fn fork_repo(&self, who: &Principal, src: RepoId, dest: &str) -> FluxResult<RepoId> {
        self.authorize(who, src, Role::Read).await?;
        let id = self.create_repo_as(who, dest, None).await?;
        // Every changeset, in log order (parents first).
        let mut ids = Vec::new();
        let mut after = 0;
        loop {
            let rows = self.meta.graph_tail(src, after, 50_000).await?;
            let Some(last) = rows.last() else { break };
            after = last.seq;
            ids.extend(rows.iter().map(|r| r.id));
        }
        self.copy_changesets(src, id, &ids).await?;
        for (name, v) in self.meta.list_refs(src, b"", 100_000).await? {
            if name.starts_with(b"pull/") || name.starts_with(b"review/") {
                continue;
            }
            self.update_ref(id, &name, RefExpect::Absent, Some(v.target), who.name(), "fork".into()).await?;
        }
        let mut cfg = (*self.repo_config(src).await?).clone();
        cfg.branch_rules = None;
        cfg.acl.clear();
        cfg.mirror = None;
        self.set_repo_config(id, &cfg).await?;
        self.meta.set_fork_of(id, src).await?;
        let src_name = self.repo_name(src).await;
        self.audit(who, "repo.fork", dest, &src_name).await;
        Ok(id)
    }

    /// Open a review on `upstream` for a stack in `fork`.
    pub async fn review_from_fork(
        &self,
        who: &Principal,
        upstream: RepoId,
        fork: RepoId,
        head: ChangesetId,
        target: &[u8],
        title: &str,
        body: &str,
    ) -> FluxResult<flux_store::Review> {
        self.authorize(who, upstream, Role::Read).await?;
        self.authorize(who, fork, Role::Read).await?;
        self.copy_missing(fork, upstream, head).await?;
        let r = self.create_review(upstream, title, body, who.name(), head, target).await?;
        self.meta.set_review_source(upstream, r.number, fork).await?;
        Ok(r)
    }

    /// Copy `head` and its ancestors missing from `to`.
    pub async fn copy_missing(&self, from: RepoId, to: RepoId, head: ChangesetId) -> FluxResult<usize> {
        let mut need = Vec::new();
        let mut seen = HashSet::new();
        let mut frontier = vec![head];
        while !frontier.is_empty() {
            let have = self.meta.get_gens(to, &frontier).await?;
            let missing: Vec<ChangesetId> =
                frontier.iter().zip(have).filter(|(id, g)| g.is_none() && seen.insert(**id)).map(|(id, _)| *id).collect();
            let mut next = Vec::new();
            for id in &missing {
                let l = self.load_one_arc(from, id).await?;
                next.extend(l.changeset.parents.iter().copied());
                need.push((l.gen_num, *id));
            }
            frontier = next;
            if need.len() > 200_000 {
                return Err(FluxError::FailedPrecondition("the fork diverged too far to review".into()));
            }
        }
        need.sort();
        let ids: Vec<ChangesetId> = need.into_iter().map(|(_, id)| id).collect();
        self.copy_changesets(from, to, &ids).await
    }

    // ───────────── lifecycle ─────────────

    pub async fn delete_repo_as(&self, who: &Principal, repo: RepoId) -> FluxResult<()> {
        self.authorize(who, repo, Role::Admin).await?;
        let name = self.repo_name(repo).await;
        self.meta.delete_repo(repo).await?;
        self.forget_repo(&name, repo).await;
        self.audit(who, "repo.delete", &name, "").await;
        Ok(())
    }

    pub async fn rename_repo_as(&self, who: &Principal, repo: RepoId, new_name: &str) -> FluxResult<()> {
        self.authorize(who, repo, Role::Admin).await?;
        let old = self.repo_name(repo).await;
        let (old_owner, _) = old.split_once('/').unwrap_or(("", &old));
        let (new_owner, short) = new_name.split_once('/').unwrap_or(("", new_name));
        if old_owner != new_owner {
            return Err(FluxError::InvalidArgument("renames keep the owner; transfer is separate".into()));
        }
        if !crate::identity::valid_repo_name(short) {
            return Err(FluxError::InvalidArgument(format!("invalid repository name {short:?}")));
        }
        self.meta.rename_repo(repo, new_name).await?;
        self.forget_repo(&old, repo).await;
        self.audit(who, "repo.rename", &old, new_name).await;
        Ok(())
    }

    // ───────────── archives ─────────────

    /// A `.tar.gz` or `.zip` of the tree at `cs`, with a `<prefix>/` root.
    pub async fn archive(&self, repo: RepoId, cs: &ChangesetId, prefix: &str, format: ArchiveFormat) -> FluxResult<Vec<u8>> {
        let root = self.root_tree(repo, cs).await?;
        let files = flux_derive::walk_tree(&self.blobs, root, &|_, _| true).await?;
        let mtime = (self.load_one_arc(repo, cs).await?.changeset.author_time.unix_ms / 1000).max(0) as u64;
        let mut contents: BTreeMap<usize, bytes::Bytes> = BTreeMap::new();
        {
            use futures::StreamExt as _;
            let futs: Vec<_> = files
                .iter()
                .enumerate()
                .filter_map(|(i, f)| f.content_id.map(|c| (i, c)))
                .map(|(i, c)| {
                    let blobs = self.blobs.clone();
                    async move { (i, blobs.get_content_bytes(&c).await) }
                })
                .collect();
            let mut s = futures::stream::iter(futs).buffered(32);
            while let Some((i, r)) = s.next().await {
                contents.insert(i, r?.unwrap_or_default());
            }
        }
        let entries = files.iter().enumerate().filter(|(i, _)| contents.contains_key(i)).map(|(i, f)| {
            let path = format!("{prefix}/{}", f.path);
            (path, f.kind, contents[&i].clone())
        });
        match format {
            ArchiveFormat::TarGz => {
                let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                for (path, kind, data) in entries {
                    write_tar_entry(&mut gz, &path, kind, &data, mtime)?;
                }
                gz.write_all(&[0u8; 1024]).map_err(|e| FluxError::Internal(e.into()))?;
                gz.finish().map_err(|e| FluxError::Internal(e.into()))
            }
            ArchiveFormat::Zip => {
                let mut z = ZipWriter::default();
                for (path, kind, data) in entries {
                    z.add(&path, kind, &data, mtime)?;
                }
                Ok(z.finish())
            }
        }
    }
}

fn octal(field: &mut [u8], v: u64) {
    let w = field.len() - 1;
    let s = format!("{v:0w$o}");
    let b = s.as_bytes();
    let src = &b[b.len().saturating_sub(w)..];
    field[..src.len()].copy_from_slice(src);
}

fn tar_header(name: &[u8], size: u64, mode: u32, typeflag: u8, link: &[u8], mtime: u64) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len().min(100)].copy_from_slice(&name[..name.len().min(100)]);
    octal(&mut h[100..108], mode as u64);
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], mtime);
    h[156] = typeflag;
    h[157..157 + link.len().min(100)].copy_from_slice(&link[..link.len().min(100)]);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|b| *b as u32).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
    h
}

fn write_tar_entry(w: &mut impl std::io::Write, path: &str, kind: FileKind, data: &[u8], mtime: u64) -> FluxResult<()> {
    let io = |e: std::io::Error| FluxError::Internal(e.into());
    let name = path.as_bytes();
    let pad = |n: usize| (512 - n % 512) % 512;
    if name.len() > 100 || (matches!(kind, FileKind::Symlink) && data.len() > 100) {
        // GNU long name / long link records.
        let mut long = |flag: u8, v: &[u8]| -> FluxResult<()> {
            let mut payload = v.to_vec();
            payload.push(0);
            w.write_all(&tar_header(b"././@LongLink", payload.len() as u64, 0o644, flag, b"", mtime)).map_err(io)?;
            w.write_all(&payload).map_err(io)?;
            w.write_all(&vec![0u8; pad(payload.len())]).map_err(io)
        };
        if name.len() > 100 {
            long(b'L', name)?;
        }
        if matches!(kind, FileKind::Symlink) && data.len() > 100 {
            long(b'K', data)?;
        }
    }
    match kind {
        FileKind::Symlink => {
            w.write_all(&tar_header(name, 0, 0o777, b'2', data, mtime)).map_err(io)?;
        }
        _ => {
            let mode = if kind.is_executable() { 0o755 } else { 0o644 };
            w.write_all(&tar_header(name, data.len() as u64, mode, b'0', b"", mtime)).map_err(io)?;
            w.write_all(data).map_err(io)?;
            w.write_all(&vec![0u8; pad(data.len())]).map_err(io)?;
        }
    }
    Ok(())
}

/// A minimal streaming-free ZIP writer (deflate, 32-bit sizes, ZIP64 not
/// needed below 4 GB).
#[derive(Default)]
struct ZipWriter {
    out: Vec<u8>,
    central: Vec<u8>,
    count: u16,
}

impl ZipWriter {
    fn add(&mut self, path: &str, kind: FileKind, data: &[u8], mtime: u64) -> FluxResult<()> {
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(data).map_err(|e| FluxError::Internal(e.into()))?;
        let comp = enc.finish().map_err(|e| FluxError::Internal(e.into()))?;
        let (method, body): (u16, &[u8]) = if comp.len() < data.len() { (8, &comp) } else { (0, data) };
        let crc = crc32(data);
        let (time, date) = dos_time(mtime);
        let offset = self.out.len() as u32;
        let name = path.as_bytes();
        let le16 = |v: u16| v.to_le_bytes();
        let le32 = |v: u32| v.to_le_bytes();
        let mut local = Vec::new();
        local.extend_from_slice(&le32(0x04034b50));
        local.extend_from_slice(&le16(20));
        local.extend_from_slice(&le16(0x0800)); // UTF-8 names
        local.extend_from_slice(&le16(method));
        local.extend_from_slice(&le16(time));
        local.extend_from_slice(&le16(date));
        local.extend_from_slice(&le32(crc));
        local.extend_from_slice(&le32(body.len() as u32));
        local.extend_from_slice(&le32(data.len() as u32));
        local.extend_from_slice(&le16(name.len() as u16));
        local.extend_from_slice(&le16(0));
        local.extend_from_slice(name);
        self.out.extend_from_slice(&local);
        self.out.extend_from_slice(body);
        let mode: u32 = match kind {
            FileKind::Symlink => 0o120777,
            k if k.is_executable() => 0o100755,
            _ => 0o100644,
        };
        let c = &mut self.central;
        c.extend_from_slice(&le32(0x02014b50));
        c.extend_from_slice(&le16(0x031e)); // made by Unix, spec 3.0
        c.extend_from_slice(&le16(20));
        c.extend_from_slice(&le16(0x0800));
        c.extend_from_slice(&le16(method));
        c.extend_from_slice(&le16(time));
        c.extend_from_slice(&le16(date));
        c.extend_from_slice(&le32(crc));
        c.extend_from_slice(&le32(body.len() as u32));
        c.extend_from_slice(&le32(data.len() as u32));
        c.extend_from_slice(&le16(name.len() as u16));
        c.extend_from_slice(&le16(0));
        c.extend_from_slice(&le16(0));
        c.extend_from_slice(&le16(0));
        c.extend_from_slice(&le16(0));
        c.extend_from_slice(&le32(mode << 16));
        c.extend_from_slice(&le32(offset));
        c.extend_from_slice(name);
        self.count = self.count.saturating_add(1);
        Ok(())
    }

    fn finish(mut self) -> Vec<u8> {
        let cd_offset = self.out.len() as u32;
        let cd_len = self.central.len() as u32;
        self.out.extend_from_slice(&self.central);
        self.out.extend_from_slice(&0x06054b50u32.to_le_bytes());
        self.out.extend_from_slice(&[0, 0, 0, 0]);
        self.out.extend_from_slice(&self.count.to_le_bytes());
        self.out.extend_from_slice(&self.count.to_le_bytes());
        self.out.extend_from_slice(&cd_len.to_le_bytes());
        self.out.extend_from_slice(&cd_offset.to_le_bytes());
        self.out.extend_from_slice(&[0, 0]);
        self.out
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut h = flate2::Crc::new();
    h.update(data);
    h.sum()
}

/// Unix seconds → MS-DOS (time, date), UTC.
fn dos_time(secs: u64) -> (u16, u16) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let year = (y - 1980).clamp(0, 127) as u16;
    let time = ((rem / 3600) as u16) << 11 | (((rem % 3600) / 60) as u16) << 5 | ((rem % 60) / 2) as u16;
    let date = year << 9 | (m as u16) << 5 | d as u16;
    (time, date)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dos_dates() {
        // 2024-02-29 12:34:56 UTC
        let (t, d) = dos_time(1_709_210_096);
        assert_eq!(d >> 9, 44);
        assert_eq!((d >> 5) & 0xf, 2);
        assert_eq!(d & 0x1f, 29);
        assert_eq!(t >> 11, 12);
        assert_eq!((t >> 5) & 0x3f, 34);
    }

    #[test]
    fn tar_checksum_is_valid() {
        let h = tar_header(b"a/b.txt", 5, 0o644, b'0', b"", 0);
        let stored = u32::from_str_radix(std::str::from_utf8(&h[148..154]).unwrap(), 8).unwrap();
        let mut copy = h;
        copy[148..156].copy_from_slice(b"        ");
        assert_eq!(stored, copy.iter().map(|b| *b as u32).sum::<u32>());
    }
}
