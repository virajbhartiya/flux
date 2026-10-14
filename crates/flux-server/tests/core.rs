//! Server write-path semantics (SPEC §7.1) without the RPC layer.

use std::collections::BTreeMap;
use std::sync::Arc;

use flux_core::{Changeset, ChangesetId, ContentId, FileChange, FileKind, RepoPath, Timestamp};
use flux_server::{Flux, FluxError};
use flux_store::{BlobStore, RefExpect, RepoId};

async fn setup() -> (Arc<Flux>, RepoId) {
    let meta = flux_store::open_meta("sqlite::memory:").await.unwrap();
    let flux = Arc::new(Flux::new(meta, BlobStore::memory()));
    let repo = flux.create_repo("r").await.unwrap();
    (flux, repo)
}

fn cs(parents: Vec<ChangesetId>, files: &[(&str, Option<(ContentId, u64)>)]) -> Changeset {
    let mut fc = BTreeMap::new();
    for (p, v) in files {
        fc.insert(
            RepoPath::try_from(*p).unwrap(),
            match v {
                Some((id, size)) => FileChange::Change { content_id: *id, size: *size, kind: FileKind::Regular, copy_from: None },
                None => FileChange::Deletion,
            },
        );
    }
    Changeset {
        parents,
        author: b"t <t@t>".to_vec(),
        author_time: Timestamp::default(),
        committer: None,
        committer_time: None,
        message: b"m\n".to_vec(),
        file_changes: fc,
        extra: BTreeMap::new(),
    }
}

#[tokio::test]
async fn create_validates_and_is_idempotent() {
    let (flux, repo) = setup().await;
    let (a, _) = flux.put_content(b"aaa").await.unwrap();

    // Missing content is rejected.
    let missing = ContentId::hash(b"never uploaded");
    let err = flux.create_changeset(repo, cs(vec![], &[("x", Some((missing, 14)))])).await.unwrap_err();
    assert!(matches!(err, FluxError::FailedPrecondition(ref m) if m.contains("CONTENT_MISSING")), "{err}");

    // Size mismatch is rejected.
    let err = flux.create_changeset(repo, cs(vec![], &[("x", Some((a, 99)))])).await.unwrap_err();
    assert!(matches!(err, FluxError::InvalidArgument(_)), "{err}");

    // Unknown parent is rejected.
    let err = flux.create_changeset(repo, cs(vec![ChangesetId::hash(b"nope")], &[])).await.unwrap_err();
    assert!(matches!(err, FluxError::FailedPrecondition(_)), "{err}");

    // Valid; and creating it twice yields the same id and log position.
    let root = cs(vec![], &[("dir/x", Some((a, 3)))]);
    let i1 = flux.create_changeset(repo, root.clone()).await.unwrap();
    let i2 = flux.create_changeset(repo, root).await.unwrap();
    assert_eq!(i1.id, i2.id);
    assert_eq!(i1.log_seq, i2.log_seq);
    assert_eq!(i1.generation, 1);

    let id1 = ChangesetId::from_slice(&i1.id).unwrap();
    let child = flux.create_changeset(repo, cs(vec![id1], &[("dir/x", None)])).await.unwrap();
    assert_eq!(child.generation, 2);
}

#[tokio::test]
async fn root_tree_is_rederived_if_mapping_lost() {
    let (flux, repo) = setup().await;
    let (a, _) = flux.put_content(b"a").await.unwrap();
    let (b, _) = flux.put_content(b"b").await.unwrap();
    let c1 = flux.create_changeset(repo, cs(vec![], &[("a", Some((a, 1)))])).await.unwrap();
    let id1 = ChangesetId::from_slice(&c1.id).unwrap();
    // Simulate a crash between insert and derive: insert a changeset directly.
    let c2 = cs(vec![id1], &[("b", Some((b, 1)))]);
    flux.meta
        .put_changesets(repo, &[flux_store::NewChangeset { id: c2.id(), parents: vec![id1], gen_num: 2, body: c2.encode() }])
        .await
        .unwrap();
    let tree = flux.root_tree(repo, &c2.id()).await.unwrap();
    let entry = flux.entry_at(repo, &c2.id(), &RepoPath::try_from("b").unwrap()).await.unwrap().unwrap();
    assert_eq!(entry.content_id(), Some(b));
    assert_eq!(flux.root_tree(repo, &c2.id()).await.unwrap(), tree);
}

#[tokio::test]
async fn revisions() {
    let (flux, repo) = setup().await;
    let (a, _) = flux.put_content(b"a").await.unwrap();
    let mut prev = vec![];
    let mut ids = vec![];
    for i in 0..4 {
        let mut c = cs(prev.clone(), &[("f", Some((a, 1)))]);
        c.message = format!("{i}\n").into_bytes();
        let info = flux.create_changeset(repo, c).await.unwrap();
        let id = ChangesetId::from_slice(&info.id).unwrap();
        ids.push(id);
        prev = vec![id];
    }
    flux.update_ref(repo, b"main", RefExpect::Absent, Some(ids[3]), "test", "t".into()).await.unwrap();
    assert_eq!(flux.resolve_rev(repo, "main").await.unwrap(), ids[3]);
    assert_eq!(flux.resolve_rev(repo, "main~2").await.unwrap(), ids[1]);
    assert_eq!(flux.resolve_rev(repo, "main^~1").await.unwrap(), ids[1]);
    assert_eq!(flux.resolve_rev(repo, &ids[2].to_hex()[..10]).await.unwrap(), ids[2]);
    assert_eq!(flux.resolve_rev(repo, &ids[2].short()).await.unwrap(), ids[2]);
    assert!(flux.resolve_rev(repo, "main~9").await.is_err());
    assert!(flux.update_ref(repo, b"bad~name", RefExpect::Any, Some(ids[0]), "test", "t".into()).await.is_err());
    assert!(flux.update_ref(repo, b"x", RefExpect::Any, Some(ChangesetId::hash(b"ghost")), "test", "t".into()).await.is_err());
}

#[tokio::test]
async fn merges_must_list_disagreeing_paths() {
    let (flux, repo) = setup().await;
    let (v1, _) = flux.put_content(b"v1").await.unwrap();
    let (v2, _) = flux.put_content(b"v2").await.unwrap();
    let base = flux.create_changeset(repo, cs(vec![], &[("a", Some((v1, 2))), ("d/b", Some((v1, 2)))])).await.unwrap();
    let base = ChangesetId::from_slice(&base.id).unwrap();
    let left = flux.create_changeset(repo, cs(vec![base], &[("a", Some((v2, 2)))])).await.unwrap();
    let left = ChangesetId::from_slice(&left.id).unwrap();
    let right = flux.create_changeset(repo, cs(vec![base], &[("d/b", Some((v2, 2)))])).await.unwrap();
    let right = ChangesetId::from_slice(&right.id).unwrap();

    // Parents disagree on `a` and `d/b`; listing neither is rejected.
    let err = flux.create_changeset(repo, cs(vec![left, right], &[])).await.unwrap_err();
    assert!(matches!(err, FluxError::InvalidArgument(ref m) if m.contains("MERGE_INCOMPLETE")), "{err}");
    let err = flux.create_changeset(repo, cs(vec![left, right], &[("d/b", Some((v2, 2)))])).await.unwrap_err();
    assert!(matches!(err, FluxError::InvalidArgument(ref m) if m.contains("MERGE_INCOMPLETE") && m.contains("a")), "{err}");
    // Listing every disagreement (even when it keeps parents[0]'s value) works.
    flux.create_changeset(repo, cs(vec![left, right], &[("a", Some((v2, 2))), ("d/b", Some((v2, 2)))])).await.unwrap();
}

#[tokio::test]
async fn copy_from_must_exist_in_parent() {
    let (flux, repo) = setup().await;
    let (v1, _) = flux.put_content(b"v1").await.unwrap();
    let base = flux.create_changeset(repo, cs(vec![], &[("a", Some((v1, 2)))])).await.unwrap();
    let base = ChangesetId::from_slice(&base.id).unwrap();
    let mut good = cs(vec![base], &[("b", Some((v1, 2)))]);
    if let Some(FileChange::Change { copy_from, .. }) = good.file_changes.values_mut().next() {
        *copy_from = Some((RepoPath::try_from("a").unwrap(), 0));
    }
    flux.create_changeset(repo, good.clone()).await.unwrap();
    let mut bad = good;
    if let Some(FileChange::Change { copy_from, .. }) = bad.file_changes.values_mut().next() {
        *copy_from = Some((RepoPath::try_from("nope").unwrap(), 0));
    }
    let err = flux.create_changeset(repo, bad).await.unwrap_err();
    assert!(matches!(err, FluxError::InvalidArgument(ref m) if m.contains("copy_from")), "{err}");
}
