//! gRPC end to end with tokens: anonymous writes are refused, protected refs
//! only fast-forward for non-admins, and landing works over the wire.

use flux_client::Client;
use flux_core::{Changeset, ContentId, FileChange, FileKind, RepoPath, Timestamp};
use flux_proto::v1::update_ref_request::Expect;
use flux_server::auth::AuthMode;
use std::collections::BTreeMap;

fn cs(parent: Option<flux_core::ChangesetId>, path: &str, id: ContentId, size: u64) -> Changeset {
    Changeset {
        parents: parent.into_iter().collect(),
        author: b"T <t@t>".to_vec(),
        author_time: Timestamp::default(),
        committer: None,
        committer_time: None,
        message: format!("{path}\n").into_bytes(),
        file_changes: BTreeMap::from([(
            RepoPath::try_from(path).unwrap(),
            FileChange::Change { content_id: id, size, kind: FileKind::Regular, copy_from: None },
        )]),
        extra: BTreeMap::new(),
    }
}

#[tokio::test]
async fn tokens_and_protected_refs() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = flux_server::StorageConfig::data_dir(dir.path()).unwrap();
    let flux = flux_server::open_with(&cfg, AuthMode::Token).await.unwrap();
    flux.create_repo("r").await.unwrap();
    let (_, user) = flux.issue_token("alice", false, "t").await.unwrap();
    let (_, admin) = flux.issue_token("root", true, "t").await.unwrap();
    let (listener, addr) = flux_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(flux_server::serve(flux, listener, std::future::pending()));
    let url = format!("http://{addr}");

    let anon = Client::connect_with_token(&url, "r", Some("")).await.unwrap();
    let alice = Client::connect_with_token(&url, "r", Some(&user)).await.unwrap();
    let root = Client::connect_with_token(&url, "r", Some(&admin)).await.unwrap();

    // Anonymous can read but not write.
    assert!(anon.list_refs(b"").await.is_ok());
    assert!(anon.upload(&[b"x"]).await.is_err());
    assert_eq!(alice.whoami().await.unwrap().user, "alice");
    assert!(alice.create_repo("other").await.is_err(), "only admins create repos");

    let ids = alice.upload(&[b"one", b"two", b"three"]).await.unwrap();
    let a = alice.create_changeset(&cs(None, "a", ids[0], 3)).await.unwrap().id;
    let b = alice.create_changeset(&cs(Some(a), "b", ids[1], 3)).await.unwrap().id;
    let side = alice.create_changeset(&cs(Some(a), "c", ids[2], 5)).await.unwrap().id;
    assert!(alice.update_ref("main", Expect::Absent(true), Some(a), "init").await.unwrap());
    // Fast-forward allowed.
    assert!(alice.update_ref("main", Expect::Any(true), Some(b), "ff").await.unwrap());
    // Non-fast-forward and deletion refused for non-admins...
    assert!(alice.update_ref("main", Expect::Any(true), Some(side), "force").await.is_err());
    assert!(alice.update_ref("main", Expect::Any(true), None, "delete").await.is_err());
    // ...but landing rebases the side change on top.
    let landed = alice.land(&side.to_hex(), "main", "land").await.unwrap();
    assert!(!landed.fast_forward);
    // Unprotected refs move freely; admins may force protected ones.
    assert!(alice.update_ref("scratch/x", Expect::Absent(true), Some(side), "s").await.unwrap());
    assert!(alice.update_ref("scratch/x", Expect::Any(true), Some(a), "s").await.unwrap());
    assert!(root.update_ref("main", Expect::Any(true), Some(a), "admin reset").await.unwrap());
}
