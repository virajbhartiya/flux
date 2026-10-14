//! Landing, reviews, history and auth, end to end against the core.

use std::collections::BTreeMap;
use std::sync::Arc;

use flux_core::{Changeset, ChangesetId, FileChange, FileKind, RepoPath, Timestamp};
use flux_server::auth::AuthMode;
use flux_server::{Flux, FluxError};
use flux_store::{BlobStore, RefExpect, RepoId};

async fn setup(mode: AuthMode) -> (Arc<Flux>, RepoId) {
    let meta = flux_store::open_meta("sqlite::memory:").await.unwrap();
    let flux = Arc::new(Flux::new(meta, BlobStore::memory()).with_auth_mode(mode));
    let repo = flux.create_repo("r").await.unwrap();
    (flux, repo)
}

async fn commit(
    flux: &Arc<Flux>,
    repo: RepoId,
    parent: Option<ChangesetId>,
    msg: &str,
    files: &[(&str, Option<&str>)],
) -> ChangesetId {
    let mut fc = BTreeMap::new();
    for (p, v) in files {
        let change = match v {
            Some(text) => {
                let (id, m) = flux.put_content(text.as_bytes()).await.unwrap();
                FileChange::Change { content_id: id, size: m.size, kind: FileKind::Regular, copy_from: None }
            }
            None => FileChange::Deletion,
        };
        fc.insert(RepoPath::try_from(*p).unwrap(), change);
    }
    let cs = Changeset {
        parents: parent.into_iter().collect(),
        author: b"T <t@t>".to_vec(),
        author_time: Timestamp { unix_ms: 1_700_000_000_000, tz_offset_min: 0 },
        committer: None,
        committer_time: None,
        message: format!("{msg}\n").into_bytes(),
        file_changes: fc,
        extra: BTreeMap::new(),
    };
    ChangesetId::from_slice(&flux.create_changeset(repo, cs).await.unwrap().id).unwrap()
}

async fn file(flux: &Arc<Flux>, repo: RepoId, cs: &ChangesetId, path: &str) -> Option<String> {
    let e = flux.entry_at(repo, cs, &RepoPath::try_from(path).unwrap()).await.unwrap()?;
    let data = flux.blobs.get_content(&e.content_id()?).await.unwrap()?;
    Some(String::from_utf8(data).unwrap())
}

#[tokio::test]
async fn land_fast_forward_rebase_merge_and_conflict() {
    let (flux, repo) = setup(AuthMode::Open).await;
    let base = commit(&flux, repo, None, "base", &[("a.txt", Some("1\n2\n3\n4\n5\n")), ("b.txt", Some("b\n"))]).await;
    flux.update_ref(repo, b"main", RefExpect::Absent, Some(base), "t", "init".into()).await.unwrap();

    // Fast-forward.
    let ff = commit(&flux, repo, Some(base), "ff", &[("c.txt", Some("c\n"))]).await;
    let r = flux.land(repo, ff, b"main", "t", "land").await.unwrap();
    assert!(r.fast_forward);
    assert_eq!(r.head, ff);

    // Main moves on; a stack based on `base` is rebased, disjoint files.
    let side = commit(&flux, repo, Some(base), "side", &[("b.txt", Some("b2\n"))]).await;
    let side2 = commit(&flux, repo, Some(side), "side2", &[("d.txt", Some("d\n"))]).await;
    let r = flux.land(repo, side2, b"main", "t", "land").await.unwrap();
    assert!(!r.fast_forward);
    assert_eq!(r.rebased.len(), 2);
    assert_eq!(file(&flux, repo, &r.head, "b.txt").await.as_deref(), Some("b2\n"));
    assert_eq!(file(&flux, repo, &r.head, "c.txt").await.as_deref(), Some("c\n"));
    assert!(flux.is_ancestor(repo, &ff, &r.head).await.unwrap());
    let head = r.head;

    // Both sides edit a.txt in separate places: three-way merge succeeds.
    let ours = commit(&flux, repo, Some(head), "ours", &[("a.txt", Some("ONE\n2\n3\n4\n5\n"))]).await;
    flux.land(repo, ours, b"main", "t", "land").await.unwrap();
    let theirs = commit(&flux, repo, Some(head), "theirs", &[("a.txt", Some("1\n2\n3\n4\nFIVE\n"))]).await;
    let r = flux.land(repo, theirs, b"main", "t", "land").await.unwrap();
    assert_eq!(r.merged_files, 1);
    assert_eq!(file(&flux, repo, &r.head, "a.txt").await.as_deref(), Some("ONE\n2\n3\n4\nFIVE\n"));

    // Overlapping edits conflict and leave main untouched.
    let before = flux.meta.get_ref(repo, b"main").await.unwrap().unwrap();
    let clash = commit(&flux, repo, Some(head), "clash", &[("a.txt", Some("uno\n2\n3\n4\n5\n"))]).await;
    let err = flux.land(repo, clash, b"main", "t", "land").await.unwrap_err();
    assert!(matches!(err, FluxError::FailedPrecondition(ref m) if m.contains("REBASE_CONFLICT")), "{err}");
    assert_eq!(flux.meta.get_ref(repo, b"main").await.unwrap().unwrap(), before);

    // History of a.txt follows the rebased changesets.
    let main = before.target;
    let hist = flux.path_history(repo, &main, &RepoPath::try_from("a.txt").unwrap(), false, 10).await.unwrap();
    assert_eq!(hist.len(), 3, "base, ours, rebased theirs");
    let blame = flux.blame(repo, &main, &RepoPath::try_from("a.txt").unwrap()).await.unwrap();
    assert_eq!(blame.lines, 5);
    assert_eq!(blame.ranges.first().unwrap().2, ours);
}

#[tokio::test]
async fn reviews_lifecycle() {
    let (flux, repo) = setup(AuthMode::Open).await;
    let base = commit(&flux, repo, None, "base", &[("a", Some("a\n"))]).await;
    flux.update_ref(repo, b"main", RefExpect::Absent, Some(base), "t", "init".into()).await.unwrap();
    let head = commit(&flux, repo, Some(base), "feature", &[("b", Some("b\n"))]).await;
    let r = flux.create_review(repo, "Add b", "body", "alice", head, b"main").await.unwrap();
    assert_eq!(r.number, 1);
    flux.update_review(repo, 1, "comment", "bob", "looks good", None).await.unwrap();
    flux.update_review(repo, 1, "approve", "bob", "", None).await.unwrap();
    let d = flux.review_detail(repo, 1).await.unwrap();
    assert_eq!(d.stack.len(), 1);
    assert_eq!(d.events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), vec!["opened", "comment", "approved"]);
    let landed = flux.land_review(repo, 1, "bob").await.unwrap();
    assert_eq!(flux.meta.get_ref(repo, b"main").await.unwrap().unwrap().target, landed.head);
    assert_eq!(flux.review(repo, 1).await.unwrap().state, "landed");
    assert!(flux.land_review(repo, 1, "bob").await.is_err(), "cannot land twice");
    assert!(flux.update_review(repo, 1, "close", "bob", "", None).await.is_err());
}

#[tokio::test]
async fn auth_modes() {
    let (flux, _) = setup(AuthMode::Token).await;
    let anon = flux.principal(None).await.unwrap();
    assert!(anon.can_read() && !anon.can_write() && !anon.is_admin());
    assert!(flux.principal(Some("flx_bogus")).await.is_err());
    let (_, token) = flux.issue_token("alice", false, "test").await.unwrap();
    let alice = flux.principal(Some(&token)).await.unwrap();
    assert!(alice.can_write() && !alice.is_admin());
    assert_eq!(alice.name(), "alice");
    let (_, admin) = flux.issue_token("root", true, "test").await.unwrap();
    assert!(flux.principal(Some(&admin)).await.unwrap().is_admin());
    flux.revoke_tokens("alice").await.unwrap();
    assert!(flux.principal(Some(&token)).await.is_err());

    let (private, _) = setup(AuthMode::Private).await;
    assert!(!private.principal(None).await.unwrap().can_read());
}

#[tokio::test]
async fn fsck_and_backup_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = flux_server::StorageConfig::data_dir(&dir.path().join("data")).unwrap();
    let flux = flux_server::open(&cfg).await.unwrap();
    let repo = flux.create_repo("r").await.unwrap();
    let a = commit(&flux, repo, None, "a", &[("x/y", Some("hello\n"))]).await;
    let b = commit(&flux, repo, Some(a), "b", &[("x/z", Some("world\n"))]).await;
    flux.update_ref(repo, b"main", RefExpect::Absent, Some(b), "t", "init".into()).await.unwrap();
    let r = flux.fsck(true, |_| {}).await.unwrap();
    assert!(r.ok(), "{:?}", r.problems);
    assert_eq!((r.changesets, r.refs), (2, 1));
    assert_eq!(r.contents, 2);

    let backup = dir.path().join("backup");
    flux.backup(&backup).await.unwrap();
    drop(flux);
    let restored = flux_server::open(&flux_server::StorageConfig::data_dir(&backup).unwrap()).await.unwrap();
    let repo = restored.repo("r").await.unwrap();
    let r = restored.fsck(true, |_| {}).await.unwrap();
    assert!(r.ok(), "{:?}", r.problems);
    assert_eq!(file(&restored, repo, &b, "x/z").await.as_deref(), Some("world\n"));
}
