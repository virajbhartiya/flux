//! Golden codec vectors (SPEC §2.3). These pin object identity across
//! platforms and releases. Regenerate only for an intentional format change:
//! `FLUX_BLESS=1 cargo test -p flux-core --test golden`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use flux_core::*;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/codec")
}

fn check(name: &str, bytes: &[u8], id: &str) {
    let path = golden_dir().join(format!("{name}.golden"));
    let actual = format!("id {id}\ncbor {}\n", hex::encode(bytes));
    if std::env::var_os("FLUX_BLESS").is_some() {
        std::fs::create_dir_all(golden_dir()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file {path:?} ({e}); run with FLUX_BLESS=1"));
    assert_eq!(actual, expected, "golden mismatch for {name}");
}

fn file(content: &[u8], kind: FileKind) -> FileChange {
    FileChange::Change { content_id: ContentId::hash(content), size: content.len() as u64, kind, copy_from: None }
}

#[test]
fn content_ids() {
    for (name, data) in [("content_empty", &b""[..]), ("content_hello", b"hello world\n")] {
        check(name, data, &ContentId::hash(data).to_string());
    }
}

#[test]
fn root_changeset() {
    let mut fc = BTreeMap::new();
    fc.insert(RepoPath::try_from("README.md").unwrap(), file(b"# flux\n", FileKind::Regular));
    fc.insert(RepoPath::try_from("bin/tool").unwrap(), file(b"#!/bin/sh\n", FileKind::Executable));
    fc.insert(RepoPath::try_from("link").unwrap(), file(b"README.md", FileKind::Symlink));
    fc.insert(RepoPath::try_from("vendor/dep").unwrap(), file(b"", FileKind::GitLink([0xab; 20])));
    let cs = Changeset {
        parents: vec![],
        author: "Ada Lovelace <ada@example.com>".into(),
        author_time: Timestamp { unix_ms: 1_760_400_000_000, tz_offset_min: 60 },
        committer: Some("CI <ci@example.com>".into()),
        committer_time: Some(Timestamp { unix_ms: 1_760_400_001_000, tz_offset_min: 0 }),
        message: b"initial\n".to_vec(),
        file_changes: fc,
        extra: BTreeMap::from([("git.encoding".to_string(), b"UTF-8".to_vec())]),
    };
    cs.validate().unwrap();
    check("changeset_root", &cs.encode(), &cs.id().to_string());
}

#[test]
fn child_changeset() {
    let mut fc = BTreeMap::new();
    fc.insert(RepoPath::try_from("README.md").unwrap(), FileChange::Deletion);
    fc.insert(
        RepoPath::try_from("docs/README.md").unwrap(),
        FileChange::Change {
            content_id: ContentId::hash(b"# flux\n"),
            size: 7,
            kind: FileKind::Regular,
            copy_from: Some((RepoPath::try_from("README.md").unwrap(), 0)),
        },
    );
    let cs = Changeset {
        parents: vec![ChangesetId::hash(b"p0"), ChangesetId::hash(b"p1")],
        author: "Grace <grace@example.com>".into(),
        author_time: Timestamp { unix_ms: -1, tz_offset_min: -720 },
        committer: None,
        committer_time: None,
        message: vec![0xff, 0x00, b'\n'],
        file_changes: fc,
        extra: BTreeMap::new(),
    };
    check("changeset_merge", &cs.encode(), &cs.id().to_string());
}

#[test]
fn trees() {
    let c = |s: &str| PathComponent::new(s).unwrap();
    let sub = Tree::new(vec![TreeEntry::file(c("main.rs"), FileKind::Regular, ContentId::hash(b"fn main(){}"), 11)]).unwrap();
    let root = Tree::new(vec![
        TreeEntry::dir(c("src"), sub.id(), sub.total_size(), sub.total_files()),
        TreeEntry::file(c("Cargo.toml"), FileKind::Regular, ContentId::hash(b"[package]"), 9),
    ])
    .unwrap();
    check("tree_empty", &Tree::empty().encode(), &Tree::empty().id().to_string());
    check("tree_root", &root.encode(), &root.id().to_string());
}

#[test]
fn merge_with_retain() {
    let mut fc = BTreeMap::new();
    fc.insert(RepoPath::try_from("kept.txt").unwrap(), FileChange::Retain);
    fc.insert(RepoPath::try_from("taken.txt").unwrap(), file(b"theirs\n", FileKind::Regular));
    let cs = Changeset {
        parents: vec![ChangesetId::hash(b"ours"), ChangesetId::hash(b"theirs")],
        author: "Merge Bot <bot@example.com>".into(),
        author_time: Timestamp { unix_ms: 1_760_400_002_000, tz_offset_min: 0 },
        committer: None,
        committer_time: None,
        message: b"merge\n".to_vec(),
        file_changes: fc,
        extra: BTreeMap::new(),
    };
    cs.validate().unwrap();
    check("changeset_merge_retain", &cs.encode(), &cs.id().to_string());
}
