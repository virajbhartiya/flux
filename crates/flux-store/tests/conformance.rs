//! MetaStore conformance suite (SPEC §14). Runs against SQLite always and
//! against Postgres when FLUX_TEST_POSTGRES_URL is set.

use std::sync::Arc;

use flux_core::{ChangesetId, ContentId};
use flux_store::*;

async fn backends() -> Vec<(&'static str, Arc<dyn MetaStore>)> {
    let mut out: Vec<(&'static str, Arc<dyn MetaStore>)> = vec![("sqlite", open_meta("sqlite::memory:").await.unwrap())];
    if let Ok(url) = std::env::var("FLUX_TEST_POSTGRES_URL") {
        out.push(("postgres", open_meta(&url).await.unwrap()));
    }
    out
}

fn unique(name: &str) -> String {
    format!("{name}-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
}

fn cs(n: u8, parents: Vec<ChangesetId>, gen_num: u64) -> NewChangeset {
    NewChangeset { id: ChangesetId::hash(&[n]), parents, gen_num, body: vec![n; 3] }
}

#[tokio::test]
async fn repos() {
    for (label, s) in backends().await {
        let name = unique("repo");
        let id = s.create_repo(&name).await.unwrap();
        assert!(matches!(s.create_repo(&name).await, Err(StoreError::RepoExists(_))), "{label}");
        assert_eq!(s.get_repo(&name).await.unwrap().unwrap().id, id, "{label}");
        assert!(s.get_repo("nope-nope").await.unwrap().is_none());
    }
}

#[tokio::test]
async fn changesets_and_log() {
    for (label, s) in backends().await {
        let repo = s.create_repo(&unique("cs")).await.unwrap();
        let a = cs(1, vec![], 1);
        let b = cs(2, vec![a.id], 2);
        let c = cs(3, vec![a.id], 2);
        let seqs = s.put_changesets(repo, &[a.clone(), b.clone()]).await.unwrap();
        assert_eq!(seqs, vec![1, 2], "{label}");
        // Idempotent re-insert keeps the original position; new ones continue.
        let seqs = s.put_changesets(repo, &[b.clone(), c.clone()]).await.unwrap();
        assert_eq!(seqs, vec![2, 3], "{label}");

        let got = s.get_changesets(repo, &[c.id, ChangesetId::hash(b"missing"), a.id]).await.unwrap();
        assert_eq!(got[0].as_ref().unwrap().gen_num, 2);
        assert!(got[1].is_none());
        assert_eq!(got[2].as_ref().unwrap().body, vec![1; 3]);

        assert_eq!(s.get_gens(repo, &[b.id]).await.unwrap(), vec![Some(2)]);
        let tail = s.log_tail(repo, 1, 10).await.unwrap();
        assert_eq!(tail, vec![(2, b.id), (3, c.id)], "{label}");

        let mut kids = s.children(repo, &a.id).await.unwrap();
        kids.sort();
        let mut want = vec![b.id, c.id];
        want.sort();
        assert_eq!(kids, want);

        let hex = b.id.to_hex();
        assert_eq!(s.resolve_prefix(repo, &hex[..8], 2).await.unwrap(), vec![b.id], "{label}");
        assert_eq!(s.resolve_prefix(repo, &hex[..7], 2).await.unwrap(), vec![b.id], "{label}");
    }
}

#[tokio::test]
async fn ref_cas() {
    for (label, s) in backends().await {
        let repo = s.create_repo(&unique("refs")).await.unwrap();
        let a = ChangesetId::hash(b"a");
        let b = ChangesetId::hash(b"b");
        let log = || RefLogEntry { actor: "test".into(), reason: "test".into() };

        assert_eq!(
            s.cas_ref(repo, b"main", RefExpect::Absent, Some(a), log()).await.unwrap(),
            CasOutcome::Updated { version: 1 }
        );
        assert!(
            matches!(
                s.cas_ref(repo, b"main", RefExpect::Absent, Some(b), log()).await.unwrap(),
                CasOutcome::Mismatch { current: Some(RefValue { version: 1, .. }) }
            ),
            "{label}"
        );
        assert!(matches!(
            s.cas_ref(repo, b"main", RefExpect::Version(7), Some(b), log()).await.unwrap(),
            CasOutcome::Mismatch { .. }
        ));
        assert_eq!(
            s.cas_ref(repo, b"main", RefExpect::Target(a), Some(b), log()).await.unwrap(),
            CasOutcome::Updated { version: 2 }
        );
        assert_eq!(s.get_ref(repo, b"main").await.unwrap(), Some(RefValue { target: b, version: 2 }));

        s.cas_ref(repo, b"scratch/x", RefExpect::Absent, Some(a), log()).await.unwrap();
        s.cas_ref(repo, b"scratch/y", RefExpect::Absent, Some(a), log()).await.unwrap();
        let listed = s.list_refs(repo, b"scratch/", 100).await.unwrap();
        assert_eq!(listed.iter().map(|(n, _)| n.as_slice()).collect::<Vec<_>>(), vec![&b"scratch/x"[..], b"scratch/y"]);

        assert_eq!(s.cas_ref(repo, b"scratch/x", RefExpect::Version(1), None, log()).await.unwrap(), CasOutcome::Deleted);
        assert!(s.get_ref(repo, b"scratch/x").await.unwrap().is_none());
    }
}

#[tokio::test]
async fn concurrent_cas_is_linearizable() {
    for (label, s) in backends().await {
        let repo = s.create_repo(&unique("race")).await.unwrap();
        let base = ChangesetId::hash(b"base");
        s.cas_ref(repo, b"main", RefExpect::Absent, Some(base), RefLogEntry { actor: "t".into(), reason: "init".into() })
            .await
            .unwrap();
        let mut handles = vec![];
        for i in 0..16u8 {
            let s = s.clone();
            handles.push(tokio::spawn(async move {
                s.cas_ref(
                    repo,
                    b"main",
                    RefExpect::Version(1),
                    Some(ChangesetId::hash(&[i])),
                    RefLogEntry { actor: "t".into(), reason: "race".into() },
                )
                .await
                .unwrap()
            }));
        }
        let mut wins = 0;
        for h in handles {
            if matches!(h.await.unwrap(), CasOutcome::Updated { .. }) {
                wins += 1;
            }
        }
        assert_eq!(wins, 1, "{label}: exactly one CAS must win");
    }
}

#[tokio::test]
async fn derived_content_git() {
    for (_label, s) in backends().await {
        let repo = s.create_repo(&unique("misc")).await.unwrap();
        let a = ChangesetId::hash(b"a");
        s.put_derived(repo, 1, &[(a, vec![9; 32])]).await.unwrap();
        assert_eq!(s.get_derived(repo, 1, &[a]).await.unwrap(), vec![Some(vec![9; 32])]);
        assert_eq!(s.get_derived(repo, 2, &[a]).await.unwrap(), vec![None]);

        let cid = ContentId::hash(b"x");
        let meta = ContentMeta { size: 1, git_sha1: [1; 20], sha256: [2; 32], is_binary: true };
        s.put_content_meta(&[(cid, meta)]).await.unwrap();
        assert_eq!(s.get_content_meta(&[cid]).await.unwrap(), vec![Some(meta)]);

        s.git_map_put(repo, &[(a, [7; 20])]).await.unwrap();
        assert_eq!(s.git_to_cs(repo, &[[7; 20], [8; 20]]).await.unwrap(), vec![Some(a), None]);
        assert_eq!(s.cs_to_git(repo, &[a]).await.unwrap(), vec![Some([7; 20])]);
    }
}
