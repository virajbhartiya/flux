//! The local mirror of the GitHub repository: a bare repository fetched
//! with mirror refspecs for branches, tags and pull request heads (not the
//! `refs/pull/*/merge` test merges). The token travels in an `http.extraHeader`
//! set through git's environment, never in the URL, argv or logs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use base64::Engine as _;

/// The `for-each-ref` patterns imported into flux.
pub const REF_PATTERNS: [&str; 3] = ["refs/heads/", "refs/tags/", "refs/pull/*/head"];

const REFSPECS: [&str; 3] = ["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*", "+refs/pull/*/head:refs/pull/*/head"];

/// `~/.cache/flux/migrate` (or the platform's cache directory).
pub fn default_root() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("flux").join("migrate"))
}

/// `<root>/<host>/<owner>/<repo>.git`.
pub fn repo_dir(root: &Path, host: &str, owner: &str, repo: &str) -> PathBuf {
    root.join(host).join(owner).join(format!("{repo}.git"))
}

fn git(dir: &Path) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("git");
    c.arg("-C").arg(dir).env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null()).kill_on_drop(true);
    c
}

async fn run(mut cmd: tokio::process::Command, what: &str, token: Option<&str>) -> Result<Vec<u8>> {
    let out = cmd.output().await.with_context(|| format!("running git {what}"))?;
    if !out.status.success() {
        let mut err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if let Some(t) = token.filter(|t| !t.is_empty()) {
            err = err.replace(t, "***");
        }
        bail!("git {what} failed: {err}");
    }
    Ok(out.stdout)
}

/// Create or update the mirror at `dir` from `url` (a URL or local path).
pub async fn sync(dir: &Path, url: &str, token: Option<&str>) -> Result<()> {
    if !dir.join("HEAD").exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut init = git(dir);
        init.args(["init", "--bare", "--quiet"]);
        run(init, "init", None).await?;
        for spec in REFSPECS {
            let mut c = git(dir);
            c.args(["config", "--add", "remote.origin.fetch", spec]);
            run(c, "config", None).await?;
        }
        let mut c = git(dir);
        c.args(["config", "remote.origin.tagOpt", "--no-tags"]);
        run(c, "config", None).await?;
    }
    let mut c = git(dir);
    c.args(["config", "remote.origin.url", url]);
    run(c, "config", None).await?;
    let mut fetch = git(dir);
    fetch.args(["fetch", "--prune", "--quiet", "origin"]);
    if let Some(t) = token {
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{t}"));
        fetch
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {basic}"));
    }
    run(fetch, "fetch", token).await?;
    Ok(())
}

/// The commits among `shas` that the mirror does not have.
pub async fn missing_commits(dir: &Path, shas: &[String]) -> Result<HashSet<String>> {
    if shas.is_empty() {
        return Ok(HashSet::new());
    }
    let mut child = git(dir).args(["cat-file", "--batch-check"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let input: String = shas.iter().map(|s| format!("{s}^{{commit}}\n")).collect();
    let mut stdin = child.stdin.take().context("git stdin")?;
    let writer = tokio::spawn(async move { tokio::io::AsyncWriteExt::write_all(&mut stdin, input.as_bytes()).await });
    let out = child.wait_with_output().await?;
    writer.await??;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(shas.iter().zip(text.lines()).filter(|(_, l)| l.ends_with(" missing")).map(|(s, _)| s.clone()).collect())
}

/// `(path, contents)` of the workflow files on `branch`.
pub async fn workflows(dir: &Path, branch: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let rev = format!("refs/heads/{branch}");
    let mut ls = git(dir);
    ls.args(["ls-tree", "--name-only", &rev, ".github/workflows/"]);
    let Ok(names) = run(ls, "ls-tree", None).await else { return Ok(vec![]) };
    let mut out = Vec::new();
    for path in String::from_utf8_lossy(&names).lines() {
        if !(path.ends_with(".yml") || path.ends_with(".yaml")) {
            continue;
        }
        let mut show = git(dir);
        show.args(["cat-file", "blob", &format!("{rev}:{path}")]);
        if let Ok(data) = run(show, "cat-file", None).await {
            out.push((path.to_string(), data));
        }
    }
    Ok(out)
}
