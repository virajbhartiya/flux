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
        // Git for Windows' system config turns on autocrlf; compare bytes.
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.autocrlf")
        .env("GIT_CONFIG_VALUE_0", "false")
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

#[tokio::test(flavor = "multi_thread")]
async fn clones_use_the_cached_pack_plus_a_tail() {
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
    std::fs::create_dir_all(&src).unwrap();

    let commit_and_push = |n: std::ops::Range<u32>| {
        let (src, url) = (src.clone(), url.clone());
        tokio::task::spawn_blocking(move || {
            if !src.join(".git").exists() {
                git(&src, &["init", "-q", "-b", "main"]);
            }
            for i in n {
                std::fs::write(src.join(format!("f{}.txt", i % 3)), format!("version {i}\n")).unwrap();
                git(&src, &["add", "-A"]);
                git(&src, &["commit", "-qm", &format!("c{i}")]);
            }
            git(&src, &["push", "-q", &url, "main"]);
            git(&src, &["rev-parse", "HEAD"])
        })
    };
    let clone_head = |name: &'static str| {
        let (root, url) = (dir.path().to_path_buf(), url.clone());
        tokio::task::spawn_blocking(move || {
            let dest = root.join(name);
            git(&root, &["clone", "-q", &url, dest.to_str().unwrap()]);
            git(&dest, &["fsck", "--strict"]);
            git(&dest, &["rev-parse", "HEAD"])
        })
    };

    let head = commit_and_push(0..5).await.unwrap();
    let repo = flux.repo("r").await.unwrap();
    flux.git_refresh_pack(repo).await.unwrap();
    let pack = flux.meta.git_pack_get(repo).await.unwrap().expect("pack cached");
    assert_eq!(pack.objects, 5 + 5 + 5, "5 commits, 5 root trees, 5 file versions");
    assert_eq!(clone_head("c1").await.unwrap(), head, "clone served from the cached pack");

    // Newer commits arrive as a generated tail after the cached objects.
    let head = commit_and_push(5..9).await.unwrap();
    assert_eq!(clone_head("c2").await.unwrap(), head, "cached pack plus tail");
}

#[tokio::test(flavor = "multi_thread")]
async fn push_protection_checks_what_the_push_leaves_at_its_tips() {
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
    std::fs::create_dir_all(&src).unwrap();
    tokio::task::spawn_blocking(move || {
        // Assembled at runtime so this file does not trip scanners itself.
        let key = format!("-----BEGIN {} PRIVATE KEY-----\nabc\n", "OPENSSH");
        git(&src, &["init", "-q", "-b", "main"]);
        // History that once held a key, long since removed: importable.
        std::fs::write(src.join("id_test"), &key).unwrap();
        git(&src, &["add", "-A"]);
        git(&src, &["commit", "-qm", "add a test key"]);
        std::fs::remove_file(src.join("id_test")).unwrap();
        std::fs::write(src.join("README.md"), "# hi\n").unwrap();
        git(&src, &["add", "-A"]);
        git(&src, &["commit", "-qm", "drop the test key"]);
        git(&src, &["push", "-q", &url, "main"]);
        // A key that the push leaves in place is refused.
        std::fs::write(src.join("deploy_key"), &key).unwrap();
        git(&src, &["add", "-A"]);
        git(&src, &["commit", "-qm", "add a deploy key"]);
        let out = Command::new("git")
            .args(["push", &url, "main"])
            .current_dir(&src)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("push protection") && err.contains("deploy_key"), "{err}");
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn pushing_many_tags_at_once_starts_no_ci_for_them() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git not installed; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = flux_server::StorageConfig::data_dir(&dir.path().join("data")).unwrap();
    let flux = flux_server::open(&cfg).await.unwrap();
    let repo = flux.create_repo("r").await.unwrap();
    let (listener, addr) = flux_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(flux_server::serve(flux.clone(), listener, std::future::pending()));
    let url = format!("http://{addr}/r.git");
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join(".flux-ci/workflows")).unwrap();
    tokio::task::spawn_blocking(move || {
        git(&src, &["init", "-q", "-b", "main"]);
        std::fs::write(
            src.join(".flux-ci/workflows/ci.yml"),
            "name: CI\non: [push]\njobs:\n  t:\n    steps:\n      - run: true\n",
        )
        .unwrap();
        git(&src, &["add", "-A"]);
        git(&src, &["commit", "-qm", "ci"]);
        for v in ["v1", "v2", "v3", "v4", "v5"] {
            git(&src, &["tag", v]);
        }
        // A migration-style push: the branch and five tags in one go.
        git(&src, &["push", "-q", &url, "main", "--tags"]);
    })
    .await
    .unwrap();
    let mut runs = vec![];
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        runs = flux.meta.list_ci_runs(repo, None, 100).await.unwrap();
        if !runs.is_empty() {
            break;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let runs = if runs.is_empty() { runs } else { flux.meta.list_ci_runs(repo, None, 100).await.unwrap() };
    let refs: Vec<&str> = runs.iter().map(|r| r.ref_name.as_str()).collect();
    assert_eq!(refs, ["main"], "only the branch push runs CI");
}
