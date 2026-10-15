//! fluxd end to end: serve objects through the daemon, then check that a
//! ref move upstream is prefetched into the daemon's cache.

use std::collections::BTreeMap;
use std::time::Duration;

use flux_client::Client;
use flux_core::{Changeset, FileChange, FileKind, ObjectKind, RepoPath, Timestamp};
use flux_proto::v1::update_ref_request::Expect;

fn commit(parent: Option<flux_core::ChangesetId>, files: &[(&str, flux_core::ContentId, u64)]) -> Changeset {
    let mut fc = BTreeMap::new();
    for (p, id, size) in files {
        fc.insert(
            RepoPath::try_from(*p).unwrap(),
            FileChange::Change { content_id: *id, size: *size, kind: FileKind::Regular, copy_from: None },
        );
    }
    Changeset {
        parents: parent.into_iter().collect(),
        author: b"Ada <ada@x.org>".to_vec(),
        author_time: Timestamp { unix_ms: 1_700_000_000_000, tz_offset_min: 0 },
        committer: None,
        committer_time: None,
        message: b"m\n".to_vec(),
        file_changes: fc,
        extra: BTreeMap::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn serves_objects_and_prefetches_new_revisions() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = flux_server::StorageConfig::data_dir(&dir.path().join("server")).unwrap();
    let flux = flux_server::open(&cfg).await.unwrap();
    flux.create_repo("r").await.unwrap();
    let (listener, addr) = flux_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(flux_server::serve(flux.clone(), listener, std::future::pending()));
    let upstream = format!("http://{addr}");

    let up = Client::connect(&upstream, "r").await.unwrap();
    let ids = up.upload(&[b"one\n", b"two\n"]).await.unwrap();
    let c1 = up.create_changeset(&commit(None, &[("a/one.txt", ids[0], 4)])).await.unwrap();
    up.update_ref("main", Expect::Any(true), Some(c1.id), "t").await.unwrap();

    let ddir = dir.path().join("fluxd");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let daemon = tokio::spawn(flux_daemon::run(
        flux_daemon::Options { upstream: upstream.clone(), dir: Some(ddir.clone()), listen: "127.0.0.1:0".parse().unwrap() },
        async move {
            let _ = stop_rx.await;
        },
    ));
    let state_path = ddir.join("fluxd.json");
    let state: flux_client::daemon::DaemonState = loop {
        if let Ok(b) = std::fs::read(&state_path)
            && let Ok(s) = serde_json::from_slice(&b)
        {
            break s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    // Reads through the daemon match the upstream.
    let via = Client::connect(&state.url, "r").await.unwrap();
    let head = via.resolve("main").await.unwrap();
    assert_eq!(head.id, c1.id);
    let trees = via.get_trees(&[head.root_tree], 64, &[], b"").await.unwrap();
    assert_eq!(trees.len(), 2, "root and a/");

    // A new revision upstream is prefetched without any client asking.
    let c2 = up.create_changeset(&commit(Some(c1.id), &[("b/two.txt", ids[1], 4)])).await.unwrap();
    up.update_ref("main", Expect::Any(true), Some(c2.id), "t").await.unwrap();
    let blobs =
        flux_store::BlobStore::open(&format!("file://{}/", ddir.join("blobs").canonicalize().unwrap().display())).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        // The daemon writes through its own handle; reopen to see them.
        let blobs =
            flux_store::BlobStore::open(&format!("file://{}/", ddir.join("blobs").canonicalize().unwrap().display())).unwrap();
        if blobs.exists(ObjectKind::Tree, c2.root_tree.as_bytes()).await.unwrap()
            && blobs.exists(ObjectKind::Content, ids[1].as_bytes()).await.unwrap()
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "new revision was not prefetched");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(blobs);

    stop_tx.send(()).unwrap();
    daemon.await.unwrap().unwrap();
    assert!(!state_path.exists(), "state file removed on shutdown");
}
