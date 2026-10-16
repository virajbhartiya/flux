//! git over SSH end to end with the real `git` and `ssh` clients: clone,
//! push, incremental fetch, tags, and key-based authorization.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use flux_server::auth::AuthMode;

fn have(bin: &str) -> bool {
    Command::new(bin).arg("-V").output().is_ok() || Command::new(bin).arg("--version").output().is_ok()
}

fn git(dir: &Path, ssh: &str, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_SSH_COMMAND", ssh)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_push_fetch_over_ssh() {
    if !have("git") || !have("ssh") || !have("ssh-keygen") {
        eprintln!("skipping: git, ssh or ssh-keygen not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = flux_server::StorageConfig::data_dir(&dir.path().join("data")).unwrap();
    let flux = flux_server::open_with(&cfg, AuthMode::Token).await.unwrap();
    flux.create_repo("demo").await.unwrap();
    let (user, _) = flux.issue_token("alice", true, "t").await.unwrap();
    let key = dir.path().join("id");
    assert!(Command::new("ssh-keygen").args(["-t", "ed25519", "-N", "", "-q", "-f"]).arg(&key).status().unwrap().success());
    let who = flux_server::auth::Principal { user: Some(user), mode: AuthMode::Token, ..Default::default() };
    flux.add_ssh_key(&who, "test", &std::fs::read_to_string(key.with_extension("pub")).unwrap()).await.unwrap();
    let other = dir.path().join("other");
    assert!(Command::new("ssh-keygen").args(["-t", "ed25519", "-N", "", "-q", "-f"]).arg(&other).status().unwrap().success());

    let host_key = flux_server::git::ssh::host_key(&dir.path().join("host_key")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(flux_server::git::ssh::serve(flux.clone(), listener, host_key, std::future::pending()));
    let ssh = |k: &Path| {
        format!(
            "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -p {port}",
            k.display()
        )
    };
    let url = format!("ssh://git@127.0.0.1:{port}/demo.git");
    let ssh_ok = ssh(&key);

    // Push a first commit into the empty repository.
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    assert!(git(&src, &ssh_ok, &["init", "-q", "-b", "main"]).status.success());
    std::fs::write(src.join("a.txt"), "one\n").unwrap();
    assert!(git(&src, &ssh_ok, &["add", "."]).status.success());
    assert!(git(&src, &ssh_ok, &["commit", "-qm", "one"]).status.success());
    let out = git(&src, &ssh_ok, &["push", "-q", &url, "main"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // Clone it back, push more, fetch incrementally.
    let c = dir.path().join("clone");
    let out = git(dir.path(), &ssh_ok, &["clone", "-q", &url, c.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read_to_string(c.join("a.txt")).unwrap(), "one\n");
    std::fs::write(src.join("a.txt"), "one\ntwo\n").unwrap();
    assert!(git(&src, &ssh_ok, &["commit", "-qam", "two"]).status.success());
    assert!(git(&src, &ssh_ok, &["tag", "v1"]).status.success());
    let out = git(&src, &ssh_ok, &["push", "-q", &url, "main", "v1"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = git(&c, &ssh_ok, &["pull", "-q", "--ff-only", "--tags"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read_to_string(c.join("a.txt")).unwrap(), "one\ntwo\n");
    let refs = String::from_utf8(git(&c, &ssh_ok, &["ls-remote", &url]).stdout).unwrap();
    assert!(refs.contains("refs/heads/main") && refs.contains("refs/tags/v1"), "{refs}");

    // An unknown key is refused.
    let out = git(dir.path(), &ssh(&other), &["ls-remote", &url]);
    assert!(!out.status.success());
}
