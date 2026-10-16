//! Git LFS with the real git-lfs client: push and clone of a chunked
//! (large) and a small object over HTTP. Skipped when git-lfs is missing.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use flux_server::auth::AuthMode;

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn lfs_push_and_clone() {
    if !Command::new("git").args(["lfs", "version"]).output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipping: git-lfs not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = flux_server::StorageConfig::data_dir(&dir.path().join("data")).unwrap();
    let flux = flux_server::open_with(&cfg, AuthMode::Token).await.unwrap();
    flux.create_repo("big").await.unwrap();
    let (_, token) = flux.issue_token("alice", true, "t").await.unwrap();
    let (listener, addr) = flux_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(flux_server::serve(flux.clone(), listener, std::future::pending()));
    let url = format!("http://alice:{token}@{addr}/big.git");

    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    for args in [&["init", "-q", "-b", "main"][..], &["lfs", "install", "--local"], &["lfs", "track", "*.bin"]] {
        assert!(git(&src, args).status.success());
    }
    let large: Vec<u8> = (0..6_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    std::fs::write(src.join("large.bin"), &large).unwrap();
    std::fs::write(src.join("small.bin"), b"small\n").unwrap();
    assert!(git(&src, &["add", "."]).status.success());
    assert!(git(&src, &["commit", "-qm", "lfs"]).status.success());
    let out = git(&src, &["push", "-q", &url, "main"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // Fetch LFS content explicitly: the machine may not have git-lfs set up
    // globally (then a plain clone leaves pointer files).
    let c = dir.path().join("clone");
    let out = git(dir.path(), &["clone", "-q", &url, c.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    for args in [&["lfs", "install", "--local"][..], &["lfs", "pull"]] {
        let out = git(&c, args);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    assert_eq!(std::fs::read(c.join("large.bin")).unwrap(), large);
    assert_eq!(std::fs::read(c.join("small.bin")).unwrap(), b"small\n");
    // The repository stores pointers; the server indexes the objects.
    let repo = flux.repo("big").await.unwrap();
    let oid = {
        use sha2::Digest as _;
        hex::encode(sha2::Sha256::digest(&large))
    };
    assert!(flux.meta.lfs_get(repo, &[oid]).await.unwrap()[0].is_some());
}
