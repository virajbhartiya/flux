//! git smart-HTTP end to end with the real git CLI: push into an empty
//! flux repository, clone it back, and check that SHA-1s survive the trip.

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@x.org")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@x.org")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn push_clone_fetch_roundtrip() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git not installed; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = flux_server::StorageConfig::data_dir(&dir.path().join("data")).unwrap();
    let flux = flux_server::open(&cfg).await.unwrap();
    flux.create_repo("r").await.unwrap();
    let (listener, addr) = flux_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(flux_server::serve(flux.clone(), listener, std::future::pending()));
    let url = format!("http://{addr}/r.git");

    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("a/b")).unwrap();
    let blocking = {
        let src = src.clone();
        let url = url.clone();
        tokio::task::spawn_blocking(move || {
            git(&src, &["init", "-q", "-b", "main"]);
            std::fs::write(src.join("README.md"), "# hi\n").unwrap();
            std::fs::write(src.join("a/b/c.txt"), "deep\n").unwrap();
            git(&src, &["add", "-A"]);
            git(&src, &["commit", "-qm", "first"]);
            git(&src, &["checkout", "-qb", "side"]);
            std::fs::write(src.join("side.txt"), "side\n").unwrap();
            git(&src, &["add", "-A"]);
            git(&src, &["commit", "-qm", "side work"]);
            git(&src, &["checkout", "-q", "main"]);
            std::fs::write(src.join("README.md"), "# hi\nmore\n").unwrap();
            git(&src, &["commit", "-qam", "second"]);
            git(&src, &["merge", "-q", "--no-edit", "side"]);
            git(&src, &["push", "-q", &url, "main", "side"]);
            let head = git(&src, &["rev-parse", "HEAD"]);
            let clone = src.parent().unwrap().join("clone");
            git(src.parent().unwrap(), &["clone", "-q", &url, clone.to_str().unwrap()]);
            assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head, "SHA-1s survive the round trip");
            git(&clone, &["fsck", "--strict"]);
            assert_eq!(std::fs::read_to_string(clone.join("a/b/c.txt")).unwrap(), "deep\n");
            // Incremental push and fetch.
            std::fs::write(src.join("new.txt"), "n\n").unwrap();
            git(&src, &["add", "-A"]);
            git(&src, &["commit", "-qm", "third"]);
            git(&src, &["push", "-q", &url, "main"]);
            git(&clone, &["pull", "-q", "--ff-only"]);
            assert_eq!(git(&clone, &["rev-parse", "HEAD"]), git(&src, &["rev-parse", "HEAD"]));
            // Legacy file modes (100664, as in early git history) keep their SHA-1s.
            let blob = git(&src, &["hash-object", "-w", "README.md"]);
            let mktree = Command::new("git")
                .args(["mktree"])
                .current_dir(&src)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            {
                use std::io::Write as _;
                let mut stdin = mktree.stdin.as_ref().unwrap();
                writeln!(stdin, "100664 blob {blob}\told.txt").unwrap();
            }
            let tree = String::from_utf8(mktree.wait_with_output().unwrap().stdout).unwrap().trim().to_string();
            let legacy = git(&src, &["commit-tree", &tree, "-m", "legacy mode"]);
            git(&src, &["push", "-q", &url, &format!("{legacy}:refs/heads/legacy")]);
            git(&clone, &["fetch", "-q", "origin", "legacy"]);
            assert_eq!(git(&clone, &["rev-parse", "FETCH_HEAD^{tree}"]), tree);
            assert_eq!(git(&clone, &["rev-parse", "FETCH_HEAD"]), legacy);
            head
        })
    };
    blocking.await.unwrap();
    let repo = flux.repo("r").await.unwrap();
    let main = flux.meta.get_ref(repo, b"main").await.unwrap().unwrap();
    let l = flux.load_one(repo, &main.target).await.unwrap();
    assert_eq!(l.changeset.message, b"third\n");
}
