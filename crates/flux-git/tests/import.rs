//! End-to-end: build a git repo with awkward history, import it through a
//! real flux server over gRPC, and verify every commit against git.

use std::path::Path;
use std::process::Command;

use flux_client::Client;
use flux_git::{ImportOptions, git};

fn sh(dir: &Path, script: &str) {
    let status = Command::new("sh")
        .arg("-ec")
        .arg(script)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Bob")
        .env("GIT_COMMITTER_EMAIL", "bob@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "script failed: {script}");
}

const FIXTURE: &str = r#"
git init -q -b main .
git config commit.gpgsign false
c() { GIT_AUTHOR_DATE="$1" GIT_COMMITTER_DATE="$1" git commit -q --allow-empty -m "$2"; }

echo hello > README.md
mkdir -p src/lib && echo 'fn main() {}' > src/main.rs && echo lib > src/lib/mod.rs
printf '#!/bin/sh\necho hi\n' > run.sh && chmod +x run.sh
ln -s README.md link
printf '\000\001\002binary' > data.bin
git add -A
c "1700000000 +0530" "initial"
test "$(git ls-tree -r HEAD | wc -l)" -eq 6

# dir -> file and file -> dir in one commit
git rm -q -r src/lib
echo now-a-file > src/lib
git rm -q README.md
mkdir README.md
echo nested > README.md/inner
git add -A
c "1700000100 -0800" "swap file and dir"
test "$(git cat-file -t HEAD:src/lib)" = blob
test "$(git cat-file -t HEAD:README.md)" = tree

# empty commit
c "1700000200 +0000" "empty"

# branch + merge with changes on both sides and a conflict resolved by hand
git checkout -q -b feature
echo feature > feature.txt && echo f > shared.txt && git add -A && c "1700000300 +0000" "feature work"
chmod -x run.sh && git add -A && c "1700000400 +0000" "drop exec bit"
git checkout -q main
echo main > main-only.txt && echo m > shared.txt && git add -A && c "1700000500 +0000" "main work"
git merge -q --no-ff --no-commit feature || true
echo resolved > shared.txt && git add -A && c "1700000600 +0000" "merge feature"

# submodule pointer (gitlink) via update-index, unicode + weird paths, deletion
git update-index --add --cacheinfo 160000,1234567890123456789012345678901234567890,vendor/dep
mkdir -p 'dir with space' && echo x > 'dir with space/é.txt' && echo y > 'a-b' && echo z > 'a.b'
git add -A && c "1700000700 +0100" "gitlink and odd paths"
git rm -q data.bin && c "1700000800 +0000" "delete binary"

# octopus-ish: second branch merged with ours strategy (merge-only paths)
git checkout -q -b side HEAD~3
echo side > side.txt && git add -A && c "1700000900 +0000" "side work"
git checkout -q main
git merge -q -s ours --no-edit side -m "ours merge"
"#;

#[tokio::test(flavor = "multi_thread")]
async fn import_and_verify_every_commit() {
    let data = tempfile::tempdir().unwrap();
    let gitdir = tempfile::tempdir().unwrap();
    sh(gitdir.path(), FIXTURE);

    let flux = flux_server::open(&flux_server::StorageConfig::data_dir(data.path()).unwrap()).await.unwrap();
    flux.create_repo("test").await.unwrap();
    let (listener, addr) = flux_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(flux_server::serve(flux, listener, async {
        let _ = stop_rx.await;
    }));

    let client = Client::connect(&format!("http://{addr}"), "test").await.unwrap();
    let mut opts = ImportOptions::new(gitdir.path());
    opts.batch = 3; // exercise batching
    opts.ref_patterns = vec!["refs/heads/".into()];
    let stats = flux_git::import(&client, &opts).await.unwrap();
    assert_eq!(stats.commits_imported, stats.commits_seen);
    assert!(stats.commits_imported >= 11, "{stats:?}");
    assert_eq!(stats.refs_updated, 3);

    // Every commit's file set matches git.
    let tips: Vec<_> = git::list_refs(gitdir.path(), &[]).unwrap().into_iter().map(|r| r.1).collect();
    let all: Vec<_> = git::rev_list(gitdir.path(), &tips).unwrap().into_iter().map(|c| c.0).collect();
    let bad = flux_git::verify(&client, gitdir.path(), &all).await.unwrap();
    assert!(bad.is_empty(), "mismatches: {bad:#?}");

    // Refs and history are navigable.
    let main = client.resolve("main").await.unwrap();
    assert_eq!(main.changeset.parents.len(), 2);
    let log = client.log("main", 0, false, None).await.unwrap();
    // Both side branches are merged into main, so main reaches every commit.
    assert_eq!(log.len(), all.len());
    let gens: Vec<u64> = log.iter().map(|c| c.generation).collect();
    assert!(gens.windows(2).all(|w| w[0] >= w[1]), "log must be in generation order");
    assert_eq!(client.read_file("main", "shared.txt".as_bytes()).await.unwrap(), b"resolved\n");
    assert_eq!(client.read_file("main~1", b"dir with space/\xc3\xa9.txt").await.unwrap(), b"x\n");
    let path_log = client.log("main", 0, false, Some(b"shared.txt")).await.unwrap();
    assert_eq!(path_log.len(), 3, "shared.txt touched by feature, main, and the merge");

    // Incremental: nothing new to import; then add a commit and re-run.
    let again = flux_git::import(&client, &opts).await.unwrap();
    assert_eq!(again.commits_imported, 0);
    sh(gitdir.path(), "echo more >> main-only.txt && git commit -qam more");
    let more = flux_git::import(&client, &opts).await.unwrap();
    assert_eq!(more.commits_imported, 1);
    let head = git::parse_sha(&String::from_utf8(git::run(gitdir.path(), &["rev-parse", "HEAD"]).unwrap()).unwrap()).unwrap();
    assert!(flux_git::verify(&client, gitdir.path(), &[head]).await.unwrap().is_empty());
    assert_eq!(client.resolve("main").await.unwrap().git_sha1, Some(head));

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
}
