//! git smart-HTTP (protocol v0/v1, stateless RPC): `git clone`, `fetch`
//! and `push` against flux repositories.
//!
//! Refs map as `refs/heads/<name>` ↔ flux `<name>` and `refs/tags/<t>` ↔
//! flux `tags/<t>`. Fetch serves deterministic git objects generated from
//! flux data (see [`super::objects`]) in a streamed packfile with OFS
//! deltas against each file's previous version. Push parses the client's
//! pack, converts new commits into changesets (verifying that they
//! regenerate to the same git SHA-1s) and moves refs with the same
//! compare-and-swap and protected-ref rules as every other writer.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use flux_core::{Changeset, ChangesetId, ContentId, FileChange, FileKind, RepoPath, Timestamp, TreeId};
use flux_store::{RefExpect, RepoId};

use super::objects::{EMPTY_TREE, Kind, Sha, Target, encode_commit, hex, parse_hex};
use super::pack::{PackObject, PackWriter, flush, pkt, pkt_str, read_pack, read_pkts, sideband};
use crate::auth::Principal;
use crate::core::{Flux, FluxError, FluxResult, is_protected_ref};

const ZERO: Sha = [0; 20];
const AGENT: &str = concat!("agent=flux/", env!("CARGO_PKG_VERSION"));
/// Largest push accepted (bytes of pack).
const MAX_PUSH: usize = 2 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    InfoRefs,
    UploadPack,
    ReceivePack,
}

/// Recognize git endpoints: `<repo>[.git]/info/refs`, `.../git-upload-pack`,
/// `.../git-receive-pack`.
pub fn route(rest: &str) -> Option<(String, Op)> {
    let rest = rest.trim_start_matches('/');
    let (base, op) = if let Some(b) = rest.strip_suffix("/info/refs") {
        (b, Op::InfoRefs)
    } else if let Some(b) = rest.strip_suffix("/git-upload-pack") {
        (b, Op::UploadPack)
    } else if let Some(b) = rest.strip_suffix("/git-receive-pack") {
        (b, Op::ReceivePack)
    } else {
        return None;
    };
    let repo = base.strip_suffix(".git").unwrap_or(base);
    (!repo.is_empty()).then(|| (repo.to_string(), op))
}

fn git_ref(flux_name: &[u8]) -> Vec<u8> {
    match flux_name.strip_prefix(b"tags/") {
        Some(t) => [b"refs/tags/".as_slice(), t].concat(),
        None => [b"refs/heads/".as_slice(), flux_name].concat(),
    }
}

fn flux_ref(git_name: &[u8]) -> Option<Vec<u8>> {
    if let Some(b) = git_name.strip_prefix(b"refs/heads/") {
        return Some(b.to_vec());
    }
    git_name.strip_prefix(b"refs/tags/").map(|t| [b"tags/".as_slice(), t].concat())
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, msg.to_string()).into_response()
}

fn unauthorized(msg: &str) -> Response {
    let mut r = (StatusCode::UNAUTHORIZED, msg.to_string()).into_response();
    r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"flux\""));
    r
}

/// Token from `Authorization: Bearer <t>` or Basic auth (password, or the
/// username when the password is empty) — what git credential helpers send.
fn token(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if let Some(t) = v.strip_prefix("Bearer ") {
        return Some(t.trim().to_string());
    }
    let b64 = v.strip_prefix("Basic ")?;
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
    let s = String::from_utf8(raw).ok()?;
    let (user, pass) = s.split_once(':').unwrap_or((&s, ""));
    Some(if pass.is_empty() { user } else { pass }.to_string())
}

pub async fn handle(flux: Arc<Flux>, repo_name: &str, op: Op, headers: &HeaderMap, query: &str, body: Body) -> Response {
    let principal = match flux.principal(token(headers).as_deref()).await {
        Ok(p) => p,
        Err(_) => return unauthorized("invalid token"),
    };
    let service = query.split('&').find_map(|kv| kv.strip_prefix("service=")).unwrap_or("");
    let write = op == Op::ReceivePack || (op == Op::InfoRefs && service == "git-receive-pack");
    if !principal.can_read() || (write && !principal.can_write()) {
        return unauthorized("authentication required");
    }
    let repo = match flux.repo(repo_name).await {
        Ok(r) => r,
        Err(_) => return err(StatusCode::NOT_FOUND, "repository not found"),
    };
    // git needs whole trees, so a principal restricted from reading any path
    // can't use plain git on this repository (flux checkouts work).
    if let Ok(cfg) = flux.repo_config(repo).await
        && cfg.acl.iter().any(|r| !cfg.allows(&principal, r.path.as_bytes(), false))
    {
        return err(
            StatusCode::FORBIDDEN,
            "this repository has path restrictions that apply to you; use `flux attach` (sparse) instead of git",
        );
    }
    let result = match op {
        Op::InfoRefs => match service {
            "git-upload-pack" | "git-receive-pack" => advertise(&flux, repo, service).await,
            _ => return err(StatusCode::FORBIDDEN, "dumb HTTP is not supported; use a recent git"),
        },
        Op::UploadPack => match axum::body::to_bytes(body, 64 << 20).await {
            Ok(b) => upload_pack(&flux, repo, &b).await,
            Err(e) => return err(StatusCode::BAD_REQUEST, &e.to_string()),
        },
        Op::ReceivePack => match axum::body::to_bytes(body, MAX_PUSH).await {
            Ok(b) => receive_pack(&flux, repo, &principal, &b).await,
            Err(e) => return err(StatusCode::PAYLOAD_TOO_LARGE, &e.to_string()),
        },
    };
    match result {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "git protocol error");
            err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

fn typed(body: impl Into<Body>, ctype: &'static str) -> Response {
    let mut r = Response::new(body.into());
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

/// Current refs as `(git ref name, git commit sha)`, plus the HEAD target.
async fn git_refs(flux: &Flux, repo: RepoId) -> FluxResult<(Vec<(Vec<u8>, Sha)>, Option<Vec<u8>>)> {
    let refs = flux.meta.list_refs(repo, b"", 100_000).await?;
    let ids: Vec<ChangesetId> = refs.iter().map(|r| r.1.target).collect();
    let shas = flux.git_commit_shas(repo, &ids).await?;
    let cfg = flux.repo_config(repo).await?;
    let names: Vec<&[u8]> = refs.iter().map(|r| r.0.as_slice()).collect();
    let head = cfg
        .default_branch
        .as_deref()
        .map(str::as_bytes)
        .filter(|d| names.contains(d))
        .or_else(|| [b"main".as_slice(), b"master", b"trunk"].into_iter().find(|d| names.contains(d)))
        .or_else(|| names.iter().copied().find(|n| !n.starts_with(b"tags/")))
        .map(git_ref);
    Ok((refs.iter().map(|r| git_ref(&r.0)).zip(shas).collect(), head))
}

async fn advertise(flux: &Flux, repo: RepoId, service: &str) -> FluxResult<Response> {
    let (refs, head) = git_refs(flux, repo).await?;
    let mut out = Vec::new();
    pkt_str(&mut out, &format!("# service={service}\n"));
    flush(&mut out);
    let caps = if service == "git-upload-pack" {
        let symref = head.as_ref().map(|h| format!(" symref=HEAD:{}", String::from_utf8_lossy(h))).unwrap_or_default();
        format!("multi_ack_detailed side-band-64k ofs-delta thin-pack no-progress include-tag{symref} {AGENT}")
    } else {
        format!("report-status delete-refs side-band-64k ofs-delta atomic quiet {AGENT}")
    };
    let mut lines: Vec<(Vec<u8>, Sha)> = Vec::new();
    if service == "git-upload-pack"
        && let Some(h) = &head
        && let Some((_, sha)) = refs.iter().find(|(n, _)| n == h)
    {
        lines.push((b"HEAD".to_vec(), *sha));
    }
    lines.extend(refs);
    if lines.is_empty() {
        pkt(&mut out, format!("{} capabilities^{{}}\0{caps}\n", hex(&ZERO)).as_bytes());
    }
    for (i, (name, sha)) in lines.iter().enumerate() {
        let mut l = format!("{} ", hex(sha)).into_bytes();
        l.extend_from_slice(name);
        if i == 0 {
            l.push(0);
            l.extend_from_slice(caps.as_bytes());
        }
        l.push(b'\n');
        pkt(&mut out, &l);
    }
    flush(&mut out);
    let ctype = if service == "git-upload-pack" {
        "application/x-git-upload-pack-advertisement"
    } else {
        "application/x-git-receive-pack-advertisement"
    };
    Ok(typed(out, ctype))
}

// ───────────── fetch ─────────────

async fn upload_pack(flux: &Arc<Flux>, repo: RepoId, body: &[u8]) -> FluxResult<Response> {
    let (lines, _) = read_pkts(body).map_err(FluxError::InvalidArgument)?;
    let mut wants: Vec<Sha> = Vec::new();
    let mut haves: Vec<Sha> = Vec::new();
    let mut caps = String::new();
    let mut done = false;
    for l in lines.into_iter().flatten() {
        let l = l.strip_suffix(b"\n").unwrap_or(&l).to_vec();
        if let Some(rest) = l.strip_prefix(b"want ") {
            let sha = parse_hex(&rest[..rest.len().min(40)]).ok_or_else(|| FluxError::InvalidArgument("bad want".into()))?;
            if wants.is_empty() && rest.len() > 41 {
                caps = String::from_utf8_lossy(&rest[41..]).into_owned();
            }
            wants.push(sha);
        } else if let Some(rest) = l.strip_prefix(b"have ") {
            if let Some(sha) = parse_hex(&rest[..rest.len().min(40)]) {
                haves.push(sha);
            }
        } else if l == b"done" {
            done = true;
        } else if l.starts_with(b"deepen") || l.starts_with(b"shallow") {
            return Err(FluxError::InvalidArgument("shallow clones are not supported yet; use flux attach --sparse".into()));
        }
    }
    let side_band = caps.split(' ').any(|c| c == "side-band-64k" || c == "side-band");
    let no_progress = caps.split(' ').any(|c| c == "no-progress");
    // Map haves we know.
    let have_cs: Vec<(Sha, ChangesetId)> = {
        let found = flux.meta.git_commits_lookup(repo, &haves).await?;
        haves.iter().zip(found).filter_map(|(s, c)| c.map(|c| (*s, c))).collect()
    };
    let mut out = Vec::new();
    if !done {
        for (s, _) in &have_cs {
            pkt_str(&mut out, &format!("ACK {} common\n", hex(s)));
        }
        if let Some((s, _)) = have_cs.last() {
            pkt_str(&mut out, &format!("ACK {} ready\n", hex(s)));
        }
        pkt_str(&mut out, "NAK\n");
        return Ok(typed(out, "application/x-git-upload-pack-result"));
    }
    match have_cs.last() {
        Some((s, _)) => pkt_str(&mut out, &format!("ACK {}\n", hex(s))),
        None => pkt_str(&mut out, "NAK\n"),
    }
    let mut want_cs = Vec::with_capacity(wants.len());
    for w in &wants {
        match flux.git_lookup_commit(repo, w).await? {
            Some(c) => want_cs.push(c),
            None => return Err(FluxError::InvalidArgument(format!("not our ref {}", hex(w)))),
        }
    }
    let plan = plan_pack(flux, repo, &want_cs, &have_cs.iter().map(|x| x.1).collect::<Vec<_>>()).await?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let _ = tx.send(Ok(Bytes::from(out))).await;
    let flux = flux.clone();
    tokio::spawn(async move {
        if let Err(e) = stream_pack(&flux, repo, plan, side_band, no_progress, &tx).await {
            let mut msg = Vec::new();
            if side_band {
                sideband(&mut msg, 3, format!("flux: {e}\n").as_bytes());
            }
            let _ = tx.send(Ok(Bytes::from(msg))).await;
        }
    });
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Ok(typed(Body::from_stream(stream), "application/x-git-upload-pack-result"))
}

/// One object to send: its git identity, where its data comes from, and an
/// optional delta base (an earlier object in the same pack).
struct Planned {
    kind: Kind,
    sha: Sha,
    src: Src,
    base: Option<(Sha, Src)>,
}

#[derive(Clone, Copy)]
enum Src {
    Commit(ChangesetId),
    Tree(TreeId),
    Blob(ContentId),
}

async fn plan_pack(flux: &Flux, repo: RepoId, wants: &[ChangesetId], haves: &[ChangesetId]) -> FluxResult<Vec<Planned>> {
    let (g, _) = flux.graph_index(repo, &wants[0]).await?;
    let mut iw = Vec::new();
    for w in wants {
        iw.push(flux.graph_index(repo, w).await?.1);
    }
    let mut ih = Vec::new();
    for h in haves {
        ih.push(flux.graph_index(repo, h).await?.1);
    }
    let mut commits: Vec<ChangesetId> = {
        let graph = g.graph.read().unwrap();
        graph.walk(&iw, &ih, false).map(|i| graph.id(i)).collect()
    };
    commits.reverse(); // oldest first, so delta bases precede
    // Everything reachable from what the client has is already there.
    let mut seen_trees: HashSet<TreeId> = HashSet::new();
    let mut seen_blobs: HashSet<ContentId> = HashSet::new();
    for h in haves.iter().take(8) {
        let root = flux.root_tree(repo, h).await?;
        mark_tree(flux, root, &mut seen_trees, &mut seen_blobs).await?;
    }
    let mut plan = Vec::new();
    let shas = flux.git_commit_shas(repo, &commits).await?;
    for (cs, sha) in commits.iter().zip(shas) {
        plan.push(Planned { kind: Kind::Commit, sha, src: Src::Commit(*cs), base: None });
        let root = flux.root_tree(repo, cs).await?;
        let parent_root = match flux.load_one_arc(repo, cs).await?.changeset.parents.first() {
            Some(p) => Some(flux.root_tree(repo, p).await?),
            None => None,
        };
        plan_tree(flux, root, parent_root, &mut seen_trees, &mut seen_blobs, &mut plan).await?;
    }
    Ok(plan)
}

async fn mark_tree(flux: &Flux, root: TreeId, trees: &mut HashSet<TreeId>, blobs: &mut HashSet<ContentId>) -> FluxResult<()> {
    let mut stack = vec![root];
    while let Some(t) = stack.pop() {
        if !trees.insert(t) {
            continue;
        }
        let tree = flux.blobs.get_tree(&t).await?.ok_or_else(|| FluxError::Internal(anyhow::anyhow!("tree {t} missing")))?;
        for e in tree.entries() {
            match (e.tree_id(), e.content_id()) {
                (Some(sub), _) => stack.push(sub),
                (None, Some(c)) => {
                    blobs.insert(c);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

async fn plan_tree(
    flux: &Flux,
    root: TreeId,
    base: Option<TreeId>,
    trees: &mut HashSet<TreeId>,
    blobs: &mut HashSet<ContentId>,
    plan: &mut Vec<Planned>,
) -> FluxResult<()> {
    // Iterative walk pairing each directory with its previous version.
    let mut stack: Vec<(TreeId, Option<TreeId>)> = vec![(root, base)];
    while let Some((t, b)) = stack.pop() {
        if !trees.insert(t) {
            continue;
        }
        let sha = flux.git_tree_sha(t).await?;
        let base_obj = match b {
            Some(bt) if trees.contains(&bt) => Some((flux.git_tree_sha(bt).await?, Src::Tree(bt))),
            _ => None,
        };
        plan.push(Planned { kind: Kind::Tree, sha, src: Src::Tree(t), base: base_obj });
        let tree = flux.blobs.get_tree(&t).await?.ok_or_else(|| FluxError::Internal(anyhow::anyhow!("tree {t} missing")))?;
        let old = match b {
            Some(bt) => flux.blobs.get_tree(&bt).await?,
            None => None,
        };
        let entries = flux.git_tree_entries(&tree).await?;
        for e in entries {
            let prev = old.as_ref().and_then(|o| o.get(&e.name).cloned());
            match e.target {
                Target::Tree(sub) => stack.push((sub, prev.and_then(|p| p.tree_id()))),
                Target::Blob(c) => {
                    if blobs.insert(c) {
                        let base = match prev.and_then(|p| p.content_id()) {
                            Some(pc) if pc != c && blobs.contains(&pc) => {
                                Some((flux.git_blob_shas(&[pc]).await?[0], Src::Blob(pc)))
                            }
                            _ => None,
                        };
                        plan.push(Planned { kind: Kind::Blob, sha: e.sha, src: Src::Blob(c), base });
                    }
                }
                Target::Gitlink => {}
            }
        }
    }
    Ok(())
}

async fn object_bytes(flux: &Flux, repo: RepoId, src: Src) -> FluxResult<Vec<u8>> {
    Ok(match src {
        Src::Commit(cs) => flux.git_commit(repo, &cs).await?.1,
        Src::Tree(t) => flux.git_tree_bytes(t).await?.0,
        Src::Blob(c) => flux
            .blobs
            .get_content_bytes(&c)
            .await?
            .ok_or_else(|| FluxError::Internal(anyhow::anyhow!("content {c} missing")))?
            .to_vec(),
    })
}

async fn stream_pack(
    flux: &Flux,
    repo: RepoId,
    plan: Vec<Planned>,
    side_band: bool,
    no_progress: bool,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> FluxResult<()> {
    let send = |data: Vec<u8>| async move {
        let mut framed = Vec::with_capacity(data.len() + 16);
        if side_band {
            sideband(&mut framed, 1, &data)
        } else {
            framed = data
        };
        tx.send(Ok(Bytes::from(framed))).await.map_err(|_| FluxError::Internal(anyhow::anyhow!("client went away")))
    };
    if side_band && !no_progress {
        let mut m = Vec::new();
        sideband(&mut m, 2, format!("flux: sending {} objects\n", plan.len()).as_bytes());
        let _ = tx.send(Ok(Bytes::from(m))).await;
    }
    let (mut w, head) = PackWriter::new(plan.len() as u32);
    send(head).await?;
    let mut buf = Vec::with_capacity(256 << 10);
    // Small LRU of recent object bytes to serve as delta bases.
    let mut recent: HashMap<Sha, Arc<Vec<u8>>> = HashMap::new();
    let mut order: std::collections::VecDeque<Sha> = std::collections::VecDeque::new();
    for p in plan {
        let data = object_bytes(flux, repo, p.src).await?;
        let base_data = match &p.base {
            Some((bsha, bsrc)) => match recent.get(bsha) {
                Some(d) => Some(d.clone()),
                None => Some(Arc::new(object_bytes(flux, repo, *bsrc).await?)),
            },
            None => None,
        };
        let bytes =
            w.object(p.kind, p.sha, &data, p.base.as_ref().map(|(s, _)| s).zip(base_data.as_deref().map(|v| v.as_slice())));
        buf.extend_from_slice(&bytes);
        if p.kind != Kind::Commit && data.len() <= (1 << 20) {
            recent.insert(p.sha, Arc::new(data));
            order.push_back(p.sha);
            if order.len() > 4096
                && let Some(old) = order.pop_front()
            {
                recent.remove(&old);
            }
        }
        if buf.len() >= 256 << 10 {
            send(std::mem::take(&mut buf)).await?;
        }
    }
    buf.extend_from_slice(&w.finish());
    send(buf).await?;
    if side_band {
        let mut end = Vec::new();
        flush(&mut end);
        let _ = tx.send(Ok(Bytes::from(end))).await;
    }
    Ok(())
}

// ───────────── push ─────────────

struct Command {
    old: Sha,
    new: Sha,
    name: Vec<u8>,
}

/// A parsed git commit (only what conversion needs).
struct GitCommit {
    tree: Sha,
    parents: Vec<Sha>,
    author: Option<(Vec<u8>, Timestamp)>,
    committer: Option<(Vec<u8>, Timestamp)>,
    raw_author: Vec<u8>,
    raw_committer: Vec<u8>,
    extra: Vec<u8>,
    message: Vec<u8>,
}

fn parse_ident(line: &[u8]) -> Option<(Vec<u8>, Timestamp)> {
    let tz_sp = line.iter().rposition(|b| *b == b' ')?;
    let tz = std::str::from_utf8(&line[tz_sp + 1..]).ok()?;
    let ts_sp = line[..tz_sp].iter().rposition(|b| *b == b' ')?;
    let secs: i64 = std::str::from_utf8(&line[ts_sp + 1..tz_sp]).ok()?.parse().ok()?;
    if tz.len() != 5 || tz == "-0000" {
        return None;
    }
    let (h, m): (i16, i16) = (tz[1..3].parse().ok()?, tz[3..5].parse().ok()?);
    let off = if tz.starts_with('-') { -(h * 60 + m) } else { h * 60 + m };
    Some((line[..ts_sp].to_vec(), Timestamp { unix_ms: secs * 1000, tz_offset_min: off }))
}

fn parse_commit(data: &[u8]) -> Result<GitCommit, String> {
    let split = data.windows(2).position(|w| w == b"\n\n");
    let (head, message) = match split {
        Some(i) => (&data[..i + 1], data[i + 2..].to_vec()),
        None => (data, vec![]),
    };
    let mut c = GitCommit {
        tree: ZERO,
        parents: vec![],
        author: None,
        committer: None,
        raw_author: vec![],
        raw_committer: vec![],
        extra: vec![],
        message,
    };
    let mut in_extra = false;
    for line in head.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        if line.starts_with(b" ") {
            if in_extra {
                c.extra.extend_from_slice(line);
                c.extra.push(b'\n');
            }
            continue;
        }
        in_extra = false;
        if let Some(r) = line.strip_prefix(b"tree ") {
            c.tree = parse_hex(r).ok_or("bad tree line")?;
        } else if let Some(r) = line.strip_prefix(b"parent ") {
            c.parents.push(parse_hex(r).ok_or("bad parent line")?);
        } else if let Some(r) = line.strip_prefix(b"author ") {
            c.raw_author = r.to_vec();
            c.author = parse_ident(r);
        } else if let Some(r) = line.strip_prefix(b"committer ") {
            c.raw_committer = r.to_vec();
            c.committer = parse_ident(r);
        } else {
            in_extra = true;
            c.extra.extend_from_slice(line);
            c.extra.push(b'\n');
        }
    }
    Ok(c)
}

/// Parsed git tree entries: (mode, name, sha).
fn parse_tree(data: &[u8]) -> Result<Vec<(u32, Vec<u8>, Sha)>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let sp = data[i..].iter().position(|b| *b == b' ').ok_or("bad tree")? + i;
        let nul = data[sp..].iter().position(|b| *b == 0).ok_or("bad tree")? + sp;
        let mode = u32::from_str_radix(std::str::from_utf8(&data[i..sp]).map_err(|_| "bad mode")?, 8).map_err(|_| "bad mode")?;
        let sha: Sha = data.get(nul + 1..nul + 21).ok_or("bad tree")?.try_into().unwrap();
        out.push((mode, data[sp + 1..nul].to_vec(), sha));
        i = nul + 21;
    }
    Ok(out)
}

/// Resolves git objects for conversion: pushed objects first, then flux.
struct Resolver<'a> {
    flux: &'a Flux,
    pushed: &'a HashMap<Sha, PackObject>,
    trees: HashMap<Sha, Vec<(u32, Vec<u8>, Sha)>>,
}

impl Resolver<'_> {
    async fn tree(&mut self, sha: &Sha) -> FluxResult<Vec<(u32, Vec<u8>, Sha)>> {
        if *sha == EMPTY_TREE {
            return Ok(vec![]);
        }
        if let Some(t) = self.trees.get(sha) {
            return Ok(t.clone());
        }
        let entries = match self.pushed.get(sha) {
            Some(o) if o.kind == Kind::Tree => parse_tree(&o.data).map_err(FluxError::InvalidArgument)?,
            _ => {
                let t = self.flux.git_tree_by_sha(sha).await?.ok_or_else(|| {
                    FluxError::InvalidArgument(format!("tree {} is not in the push or the repository", hex(sha)))
                })?;
                let (bytes, _) = self.flux.git_tree_bytes(t).await?;
                parse_tree(&bytes).map_err(|e| FluxError::Internal(anyhow::anyhow!(e)))?
            }
        };
        self.trees.insert(*sha, entries.clone());
        Ok(entries)
    }
}

/// Paths whose (mode, sha) differ between two git trees.
async fn diff(r: &mut Resolver<'_>, a: Option<Sha>, b: Option<Sha>) -> FluxResult<Vec<(Vec<u8>, Option<(u32, Sha)>)>> {
    let mut out = Vec::new();
    let mut stack: Vec<(Vec<u8>, Option<Sha>, Option<Sha>)> = vec![(vec![], a, b)];
    while let Some((prefix, ta, tb)) = stack.pop() {
        if ta == tb {
            continue;
        }
        let ea = match ta {
            Some(s) => r.tree(&s).await?,
            None => vec![],
        };
        let eb = match tb {
            Some(s) => r.tree(&s).await?,
            None => vec![],
        };
        let ma: HashMap<&[u8], (u32, Sha)> = ea.iter().map(|(m, n, s)| (n.as_slice(), (*m, *s))).collect();
        let mb: HashMap<&[u8], (u32, Sha)> = eb.iter().map(|(m, n, s)| (n.as_slice(), (*m, *s))).collect();
        let mut names: Vec<&[u8]> = ma.keys().chain(mb.keys()).copied().collect();
        names.sort();
        names.dedup();
        for name in names {
            let (va, vb) = (ma.get(name).copied(), mb.get(name).copied());
            if va == vb {
                continue;
            }
            let path = if prefix.is_empty() { name.to_vec() } else { [prefix.as_slice(), b"/", name].concat() };
            let dir = |v: Option<(u32, Sha)>| v.filter(|(m, _)| *m == 0o040000).map(|(_, s)| s);
            let file = |v: Option<(u32, Sha)>| v.filter(|(m, _)| *m != 0o040000);
            if dir(va).is_some() || dir(vb).is_some() {
                stack.push((path.clone(), dir(va), dir(vb)));
            }
            let (fa, fb) = (file(va), file(vb));
            if fa != fb {
                if fb.is_none() && dir(vb).is_some() {
                    // file → directory: the files underneath replace it.
                    out.push((path, None));
                } else {
                    out.push((path, fb));
                }
            }
        }
    }
    Ok(out)
}

async fn receive_pack(flux: &Arc<Flux>, repo: RepoId, principal: &Principal, body: &[u8]) -> FluxResult<Response> {
    let (lines, pack_at) = read_pkts(body).map_err(FluxError::InvalidArgument)?;
    let mut cmds = Vec::new();
    let mut caps = String::new();
    for l in lines.into_iter().flatten() {
        let (main, c) = match l.iter().position(|b| *b == 0) {
            Some(i) => (l[..i].to_vec(), String::from_utf8_lossy(&l[i + 1..]).into_owned()),
            None => (l.clone(), String::new()),
        };
        if cmds.is_empty() {
            caps = c;
        }
        let main = main.strip_suffix(b"\n").unwrap_or(&main).to_vec();
        let parts: Vec<&[u8]> = main.splitn(3, |b| *b == b' ').collect();
        if parts.len() != 3 {
            continue;
        }
        let (Some(old), Some(new)) = (parse_hex(parts[0]), parse_hex(parts[1])) else { continue };
        cmds.push(Command { old, new, name: parts[2].to_vec() });
    }
    let side_band = caps.split(' ').any(|c| c == "side-band-64k");
    let atomic = caps.split(' ').any(|c| c == "atomic");
    let pack = &body[pack_at..];
    let mut unpack = Ok(HashMap::new());
    if !pack.is_empty() {
        unpack = unpack_with_bases(flux, repo, pack).await;
    }
    let mut status: Vec<(Vec<u8>, Result<(), String>)> = Vec::new();
    match &unpack {
        Err(e) => {
            for c in &cmds {
                status.push((c.name.clone(), Err(format!("unpacker error: {e}"))));
            }
        }
        Ok(objs) => {
            // Convert all new commits first; atomic pushes fail as a whole.
            let mut converted: Result<HashMap<Sha, ChangesetId>, String> = Ok(HashMap::new());
            let tips: Vec<Sha> = cmds.iter().filter(|c| c.new != ZERO).map(|c| c.new).collect();
            if !tips.is_empty() {
                converted = import_commits(flux, repo, principal, objs, &tips).await.map_err(|e| e.to_string());
            }
            for c in &cmds {
                let r = match &converted {
                    Err(e) => Err(e.clone()),
                    Ok(map) => update_one(flux, repo, principal, c, map).await.map_err(|e| e.to_string()),
                };
                status.push((c.name.clone(), r));
                if atomic && status.last().is_some_and(|s| s.1.is_err()) {
                    break;
                }
            }
        }
    }
    let mut report = Vec::new();
    match &unpack {
        Ok(_) => pkt_str(&mut report, "unpack ok\n"),
        Err(e) => pkt_str(&mut report, &format!("unpack {e}\n")),
    }
    for (name, r) in &status {
        let mut l = match r {
            Ok(()) => b"ok ".to_vec(),
            Err(_) => b"ng ".to_vec(),
        };
        l.extend_from_slice(name);
        if let Err(e) = r {
            l.push(b' ');
            l.extend_from_slice(e.replace('\n', " ").as_bytes());
        }
        l.push(b'\n');
        pkt(&mut report, &l);
    }
    flush(&mut report);
    let mut out = Vec::new();
    if side_band {
        sideband(&mut out, 1, &report);
        flush(&mut out);
    } else {
        out = report;
    }
    Ok(typed(out, "application/x-git-receive-pack-result"))
}

/// Parse a pushed pack; thin-pack bases are fetched from flux on demand.
async fn unpack_with_bases(flux: &Flux, repo: RepoId, pack: &[u8]) -> Result<HashMap<Sha, PackObject>, String> {
    let mut known: HashMap<Sha, (Kind, Vec<u8>)> = HashMap::new();
    for _round in 0..4 {
        let mut asked: Vec<Sha> = Vec::new();
        let r = read_pack(pack, |s| match known.get(s) {
            Some(v) => Some(v.clone()),
            None => {
                asked.push(*s);
                None
            }
        });
        match r {
            Ok(objs) => return Ok(objs.into_iter().map(|o| (o.sha, o)).collect()),
            Err(e) if asked.is_empty() => return Err(e),
            Err(_) => {
                for s in asked {
                    if let Some(v) = existing_object(flux, repo, &s).await.map_err(|e| e.to_string())? {
                        known.insert(s, v);
                    }
                }
            }
        }
    }
    Err("could not resolve thin-pack bases".into())
}

async fn existing_object(flux: &Flux, repo: RepoId, sha: &Sha) -> FluxResult<Option<(Kind, Vec<u8>)>> {
    if let Some(c) = flux.meta.content_by_sha1(std::slice::from_ref(sha)).await?.pop().flatten() {
        return Ok(flux.blobs.get_content_bytes(&c).await?.map(|b| (Kind::Blob, b.to_vec())));
    }
    if let Some(t) = flux.git_tree_by_sha(sha).await? {
        return Ok(Some((Kind::Tree, flux.git_tree_bytes(t).await?.0)));
    }
    if let Some(cs) = flux.git_lookup_commit(repo, sha).await? {
        return Ok(Some((Kind::Commit, flux.git_commit(repo, &cs).await?.1)));
    }
    Ok(None)
}

/// Convert pushed commits reachable from `tips` into changesets (parents
/// first). Returns git sha → changeset for every tip and new commit.
async fn import_commits(
    flux: &Arc<Flux>,
    repo: RepoId,
    principal: &Principal,
    objs: &HashMap<Sha, PackObject>,
    tips: &[Sha],
) -> FluxResult<HashMap<Sha, ChangesetId>> {
    let mut map: HashMap<Sha, ChangesetId> = HashMap::new();
    // Peel annotated tags to their commits.
    let mut tip_commits = Vec::new();
    for t in tips {
        let mut s = *t;
        while let Some(o) = objs.get(&s).filter(|o| o.kind == Kind::Tag) {
            let target = o
                .data
                .strip_prefix(b"object ")
                .and_then(|r| parse_hex(&r[..40]))
                .ok_or_else(|| FluxError::InvalidArgument("bad tag".into()))?;
            map.insert(s, ChangesetId::from_bytes([0; 32]));
            s = target;
        }
        tip_commits.push((*t, s));
    }
    // Topological order of new commits (DFS, parents first).
    let mut order: Vec<Sha> = Vec::new();
    let mut state: HashMap<Sha, u8> = HashMap::new();
    for (_, c) in &tip_commits {
        let mut stack = vec![(*c, false)];
        while let Some((s, expanded)) = stack.pop() {
            if expanded {
                state.insert(s, 2);
                order.push(s);
                continue;
            }
            if state.contains_key(&s) {
                continue;
            }
            if let Some(cs) = flux.git_lookup_commit(repo, &s).await? {
                map.insert(s, cs);
                state.insert(s, 2);
                continue;
            }
            let o = objs.get(&s).ok_or_else(|| FluxError::InvalidArgument(format!("commit {} missing from push", hex(&s))))?;
            if o.kind != Kind::Commit {
                return Err(FluxError::InvalidArgument(format!("{} is not a commit", hex(&s))));
            }
            state.insert(s, 1);
            stack.push((s, true));
            for p in parse_commit(&o.data).map_err(FluxError::InvalidArgument)?.parents {
                if !state.contains_key(&p) {
                    stack.push((p, false));
                }
            }
        }
    }
    let mut r = Resolver { flux, pushed: objs, trees: HashMap::new() };
    let mut batch: Vec<(Sha, Changeset, Sha)> = Vec::new();
    for s in &order {
        let raw = &objs[s].data;
        let c = parse_commit(raw).map_err(FluxError::InvalidArgument)?;
        let parents: Vec<ChangesetId> = c
            .parents
            .iter()
            .map(|p| map.get(p).copied().ok_or_else(|| FluxError::InvalidArgument(format!("unknown parent {}", hex(p)))))
            .collect::<FluxResult<_>>()?;
        // File changes: against the first parent, plus merge disagreements.
        let p0_tree = match c.parents.first() {
            Some(p) => Some(commit_tree(flux, repo, objs, p).await?),
            None => None,
        };
        let mut changes: std::collections::BTreeMap<RepoPath, FileChange> = std::collections::BTreeMap::new();
        let mut blobs_needed: Vec<(RepoPath, u32, Sha)> = Vec::new();
        for (path, v) in diff(&mut r, p0_tree, Some(c.tree)).await? {
            let rp = RepoPath::new(path).map_err(|e| FluxError::InvalidArgument(e.to_string()))?;
            match v {
                None => {
                    changes.insert(rp, FileChange::Deletion);
                }
                Some((mode, sha)) => blobs_needed.push((rp, mode, sha)),
            }
        }
        for p in c.parents.iter().skip(1) {
            let pt = commit_tree(flux, repo, objs, p).await?;
            for (path, _) in diff(&mut r, p0_tree, Some(pt)).await? {
                let rp = RepoPath::new(path).map_err(|e| FluxError::InvalidArgument(e.to_string()))?;
                if changes.contains_key(&rp) || blobs_needed.iter().any(|b| b.0 == rp) {
                    continue;
                }
                // The merge kept the first parent's value (or its absence).
                let in_p0 = path_in(&mut r, p0_tree, &rp).await?.is_some();
                changes.insert(rp, if in_p0 { FileChange::Retain } else { FileChange::Deletion });
            }
        }
        // Store new blob contents; existing ones map by sha.
        let shas: Vec<Sha> = blobs_needed.iter().filter(|b| b.1 != 0o160000).map(|b| b.2).collect();
        let existing: HashMap<Sha, ContentId> =
            shas.iter().copied().zip(flux.meta.content_by_sha1(&shas).await?).filter_map(|(s, c)| c.map(|c| (s, c))).collect();
        let mut uploads: Vec<(Bytes, Option<ContentId>)> = Vec::new();
        let mut upload_shas: Vec<Sha> = Vec::new();
        for (_, mode, sha) in &blobs_needed {
            if *mode == 0o160000 || existing.contains_key(sha) || upload_shas.contains(sha) {
                continue;
            }
            let o = objs.get(sha).ok_or_else(|| FluxError::InvalidArgument(format!("blob {} missing from push", hex(sha))))?;
            uploads.push((Bytes::from(o.data.clone()), None));
            upload_shas.push(*sha);
        }
        let stored = flux.put_contents(uploads).await?;
        let mut contents = existing;
        for (s, (cid, _)) in upload_shas.iter().zip(&stored) {
            contents.insert(*s, *cid);
        }
        let sizes: HashMap<ContentId, u64> = {
            let ids: Vec<ContentId> = contents.values().copied().collect();
            ids.iter()
                .copied()
                .zip(flux.meta.get_content_meta(&ids).await?)
                .map(|(id, m)| (id, m.map(|m| m.size).unwrap_or(0)))
                .collect()
        };
        for (rp, mode, sha) in blobs_needed {
            let kind = match mode {
                0o120000 => FileKind::Symlink,
                0o160000 => FileKind::GitLink(sha),
                m if m & !0o777 == 0o100000 => FileKind::from_git_file_mode(m),
                m => return Err(FluxError::InvalidArgument(format!("unsupported git mode {m:o} at {rp}"))),
            };
            let change = match kind {
                FileKind::GitLink(_) => FileChange::Change { content_id: ContentId::empty(), size: 0, kind, copy_from: None },
                _ => {
                    let cid = contents[&sha];
                    FileChange::Change { content_id: cid, size: sizes[&cid], kind, copy_from: None }
                }
            };
            changes.insert(rp, change);
        }
        let mut extra = std::collections::BTreeMap::new();
        if !c.extra.is_empty() {
            extra.insert("git.extra_headers".to_string(), c.extra.clone());
        }
        let (author, author_time, committer, committer_time) = match (&c.author, &c.committer) {
            (Some(a), Some(cm)) => (a.0.clone(), a.1, Some(cm.0.clone()), Some(cm.1)),
            _ => {
                extra.insert("git.raw_author".to_string(), c.raw_author.clone());
                extra.insert("git.raw_committer".to_string(), c.raw_committer.clone());
                (Vec::new(), Timestamp::default(), Some(Vec::new()), Some(Timestamp::default()))
            }
        };
        let paths: Vec<Vec<u8>> = changes.keys().map(|p| p.as_bytes().to_vec()).collect();
        flux.check_write_paths(repo, principal, paths).await?;
        let mut cs = Changeset {
            parents,
            author,
            author_time,
            committer,
            committer_time,
            message: c.message.clone(),
            file_changes: changes,
            extra,
        };
        // Byte-exact round trip, or keep the verbatim commit.
        if encode_commit(&cs, &c.tree, &c.parents) != *raw {
            cs.extra.insert("git.commit".to_string(), raw.clone());
        }
        let id = cs.id();
        map.insert(*s, id);
        batch.push((*s, cs, c.tree));
    }
    if !batch.is_empty() {
        let changesets: Vec<Changeset> = batch.iter().map(|b| b.1.clone()).collect();
        flux.create_changesets(repo, changesets).await?;
        // Every pushed tree must regenerate to the same git tree.
        for (sha, _, tree) in &batch {
            let cs = map[sha];
            let ours = flux.git_tree_sha(flux.root_tree(repo, &cs).await?).await?;
            if ours != *tree {
                return Err(FluxError::InvalidArgument(format!(
                    "commit {} has a non-canonical tree (git would hash it to {}); re-create it with a modern git",
                    hex(sha),
                    hex(&ours)
                )));
            }
        }
        let pairs: Vec<(ChangesetId, Sha)> = batch.iter().map(|(s, _, _)| (map[s], *s)).collect();
        flux.meta.git_commits_put(repo, &pairs).await?;
        flux.meta.git_map_put(repo, &pairs).await?;
        for (cs, sha) in &pairs {
            flux.git_caches().commits.insert((repo, *cs), *sha);
        }
        tracing::info!(repo = repo.0, commits = batch.len(), user = principal.name(), "git push imported");
    }
    for (t, c) in tip_commits {
        if t != c {
            let cs = map[&c];
            map.insert(t, cs);
        }
    }
    Ok(map)
}

async fn commit_tree(flux: &Flux, repo: RepoId, objs: &HashMap<Sha, PackObject>, commit: &Sha) -> FluxResult<Sha> {
    if let Some(o) = objs.get(commit).filter(|o| o.kind == Kind::Commit) {
        return Ok(parse_commit(&o.data).map_err(FluxError::InvalidArgument)?.tree);
    }
    let cs = flux
        .git_lookup_commit(repo, commit)
        .await?
        .ok_or_else(|| FluxError::InvalidArgument(format!("unknown commit {}", hex(commit))))?;
    Ok(flux.git_commit(repo, &cs).await?.2)
}

async fn path_in(r: &mut Resolver<'_>, tree: Option<Sha>, path: &RepoPath) -> FluxResult<Option<(u32, Sha)>> {
    let Some(mut t) = tree else { return Ok(None) };
    let comps: Vec<&[u8]> = path.components().collect();
    for (i, c) in comps.iter().enumerate() {
        let entries = r.tree(&t).await?;
        let Some((m, _, s)) = entries.iter().find(|(_, n, _)| n.as_slice() == *c) else { return Ok(None) };
        if i + 1 == comps.len() {
            return Ok(Some((*m, *s)));
        }
        if *m != 0o040000 {
            return Ok(None);
        }
        t = *s;
    }
    Ok(None)
}

async fn update_one(
    flux: &Arc<Flux>,
    repo: RepoId,
    principal: &Principal,
    c: &Command,
    map: &HashMap<Sha, ChangesetId>,
) -> FluxResult<()> {
    let name =
        flux_ref(&c.name).ok_or_else(|| FluxError::InvalidArgument("only refs/heads/* and refs/tags/* can be pushed".into()))?;
    let current = flux.meta.get_ref(repo, &name).await?;
    let current_sha = match &current {
        Some(v) => Some(flux.git_commit_shas(repo, &[v.target]).await?[0]),
        None => None,
    };
    let expect = match (c.old == ZERO, current_sha) {
        (true, None) => RefExpect::Absent,
        (false, Some(s)) if s == c.old => RefExpect::Version(current.unwrap().version),
        _ => return Err(FluxError::FailedPrecondition("fetch first: the ref moved".into())),
    };
    let new = if c.new == ZERO {
        None
    } else {
        Some(*map.get(&c.new).ok_or_else(|| FluxError::Internal(anyhow::anyhow!("commit not converted")))?)
    };
    if is_protected_ref(&name) && !principal.is_admin() {
        match (new, &current) {
            (None, _) => return Err(FluxError::PermissionDenied("protected ref: only admins can delete it".into())),
            (Some(n), Some(cur)) if !flux.is_ancestor(repo, &cur.target, &n).await? => {
                return Err(FluxError::PermissionDenied("protected ref: non-fast-forward (use flux land)".into()));
            }
            _ => {}
        }
    }
    let (outcome, _) = flux.update_ref(repo, &name, expect, new, principal.name(), "git push".into()).await?;
    if matches!(outcome, flux_store::CasOutcome::Mismatch { .. }) {
        return Err(FluxError::FailedPrecondition("fetch first: the ref moved".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        assert_eq!(route("org/app.git/info/refs"), Some(("org/app".into(), Op::InfoRefs)));
        assert_eq!(route("app/git-upload-pack"), Some(("app".into(), Op::UploadPack)));
        assert_eq!(route("app.git/git-receive-pack"), Some(("app".into(), Op::ReceivePack)));
        assert_eq!(route("app/-/tree/main"), None);
        assert_eq!(git_ref(b"main"), b"refs/heads/main");
        assert_eq!(flux_ref(b"refs/tags/v1").unwrap(), b"tags/v1");
    }

    #[test]
    fn commit_parse_roundtrip_fields() {
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <a@x> 1700000000 +0530\ncommitter B <b@x> 1700000001 -0800\n\nmsg\n";
        let c = parse_commit(raw).unwrap();
        assert_eq!(c.tree, EMPTY_TREE);
        assert_eq!(c.author.as_ref().unwrap().1.tz_offset_min, 330);
        assert_eq!(c.message, b"msg\n");
    }
}
