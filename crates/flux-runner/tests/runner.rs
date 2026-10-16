//! End to end: a real flux server in-process, a workflow committed through
//! the API, and the `flux-runner` binary running its jobs with `--once`.
#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use flux_server::auth::AuthMode;
use serde_json::{Value, json};

struct Api {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl Api {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}/api/v1/{path}", self.base)).bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
    async fn get(&self, path: &str) -> (u16, Value) {
        self.call(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }
    async fn text(&self, path: &str) -> String {
        let resp = self.http.get(format!("{}/api/v1/{path}", self.base)).bearer_auth(&self.token).send().await.unwrap();
        resp.text().await.unwrap()
    }
}

struct Harness {
    dir: tempfile::TempDir,
    api: Api,
    runner_token: String,
}

impl Harness {
    /// A flux server with a private repository `app` (with a secret) and a
    /// registered runner.
    async fn start() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let cfg = flux_server::StorageConfig::data_dir(&dir.path().join("data")).unwrap();
        let flux = flux_server::open_with(&cfg, AuthMode::Token).await.unwrap();
        let (listener, addr) = flux_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let base = format!("http://{addr}");
        let flux = Arc::new(Arc::try_unwrap(flux).ok().unwrap().with_settings(flux_server::settings::Settings {
            public_url: Some(base.clone()),
            secret_key: Some([9u8; 32]),
            ..Default::default()
        }));
        let (_, root) = flux.issue_token("root", true, "t").await.unwrap();
        tokio::spawn(flux_server::serve(flux.clone(), listener, std::future::pending()));
        let api = Api { base, token: root, http: reqwest::Client::new() };
        let (s, v) = api.post("repos", json!({ "name": "app", "visibility": "private" })).await;
        assert_eq!(s, 201, "{v}");
        let (s, v) =
            api.call(reqwest::Method::PUT, "repos/app/secrets/DEPLOY_KEY", Some(json!({ "value": "hunter2-secret" }))).await;
        assert_eq!(s, 204, "{v}");
        let (s, runner) = api.post("runners", json!({ "name": "test-runner", "labels": ["flux-test"] })).await;
        assert_eq!(s, 201, "{runner}");
        let runner_token = runner["token"].as_str().unwrap().to_string();
        Harness { dir, api, runner_token }
    }

    /// Commit a workflow to `main` and wait until its run is queued.
    async fn commit(&self, workflow: &str) -> i64 {
        let (s, v) = self
            .api
            .post(
                "repos/app/changes",
                json!({
                    "branch": "main",
                    "message": "add CI",
                    "files": [
                        { "path": "README.md", "content": "hello from flux" },
                        { "path": ".flux-ci/workflows/ci.yml", "content": workflow },
                    ],
                }),
            )
            .await;
        assert_eq!(s, 201, "{v}");
        // CI is triggered asynchronously.
        for _ in 0..200 {
            let (_, runs) = self.api.get("repos/app/runs").await;
            if let Some(id) = runs.as_array().and_then(|r| r.first()).and_then(|r| r["id"].as_i64()) {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no CI run was queued");
    }

    fn work_dir(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    /// Start `flux-runner --once` (it exits when no job is waiting).
    fn spawn_runner(&self) -> tokio::process::Child {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_flux-runner"))
            .args(["--server", &self.api.base, "--labels", "flux-test", "--once", "--wait", "2", "--name", "itest"])
            .arg("--work-dir")
            .arg(self.work_dir())
            .env("FLUX_RUNNER_TOKEN", &self.runner_token)
            .env("RUST_LOG", "warn")
            .spawn()
            .unwrap()
    }

    async fn wait_runner(child: &mut tokio::process::Child, limit: Duration) {
        let status = tokio::time::timeout(limit, child.wait()).await.expect("the runner finished in time").unwrap();
        assert!(status.success(), "runner exited with {status}");
    }

    async fn run_runner(&self) {
        Self::wait_runner(&mut self.spawn_runner(), Duration::from_secs(300)).await;
    }

    /// Wait until the run's first job logged `text`; returns the job id.
    async fn wait_for_log(&self, run_id: i64, text: &str) -> i64 {
        for _ in 0..400 {
            let (_, jobs) = self.jobs(run_id).await;
            if let Some((job, _)) = jobs.iter().find(|(_, log)| log.contains(text)) {
                return job["id"].as_i64().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no job logged {text:?}");
    }

    /// The run and its jobs with their logs.
    async fn jobs(&self, run_id: i64) -> (Value, Vec<(Value, String)>) {
        let (_, run) = self.api.get(&format!("repos/app/runs/{run_id}")).await;
        let mut out = Vec::new();
        for job in run["jobs"].as_array().unwrap() {
            let log = self.api.text(&format!("repos/app/jobs/{}/log", job["id"])).await;
            out.push((job.clone(), log));
        }
        (run, out)
    }

    fn assert_cleaned_up(&self) {
        let leftovers: Vec<_> = std::fs::read_dir(self.work_dir()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}

const WORKFLOW: &str = r#"name: CI
on: [push]
jobs:
  build:
    runs-on: [self-hosted, flux-test]
    strategy:
      matrix:
        greeting: [hello, bonjour]
    env:
      GREETING: ${{ matrix.greeting }}
    steps:
      - uses: actions/checkout@v4
      - id: x
        name: Produce outputs
        run: |
          echo "y=from-x-$GREETING" >> "$GITHUB_OUTPUT"
          echo "ADDED=added" >> "$GITHUB_ENV"
          mkdir -p "$RUNNER_TEMP/bin"
          printf '#!/bin/sh\necho tool ran\n' > "$RUNNER_TEMP/bin/mytool"
          chmod +x "$RUNNER_TEMP/bin/mytool"
          echo "$RUNNER_TEMP/bin" >> "$GITHUB_PATH"
          echo "README says: $(cat README.md)"
          echo "secret is ${{ secrets.DEPLOY_KEY }}"
      - run: echo "got ${{ steps.x.outputs.y }} and env $ADDED on ${{ runner.os }}" && mytool
      - name: Allowed to fail
        id: flaky
        run: exit 3
        continue-on-error: true
      - if: steps.flaky.outcome == 'failure' && matrix.greeting == 'hello'
        run: echo "flaky failed as expected"
      - uses: actions/setup-node@v4
      - if: failure()
        run: echo "should not run"
      - name: Status
        if: ${{ success() }}
        run: echo "job status is ${{ job.status }}, conclusion of flaky is ${{ steps.flaky.conclusion }}"
  skipped:
    if: github.event_name == 'pull_request'
    runs-on: [self-hosted, flux-test]
    steps:
      - run: echo never
"#;

#[tokio::test(flavor = "multi_thread")]
async fn runs_a_workflow_end_to_end() {
    let h = Harness::start().await;
    let run_id = h.commit(WORKFLOW).await;
    h.run_runner().await;

    let (_, st) = h.api.get("repos/app/commits/main/status?wait=5").await;
    assert_eq!(st["state"], "success", "{st}");
    assert_eq!(st["total_count"], 3, "{st}");

    let (run, jobs) = h.jobs(run_id).await;
    assert_eq!(run["state"], "success", "{run}");
    assert_eq!(jobs.len(), 3, "{run}");
    for (job, log) in &jobs {
        let name = job["name"].as_str().unwrap();
        if name == "skipped" {
            assert_eq!(job["state"], "skipped", "{job}");
            assert!(log.contains("Skipping the job"), "{log}");
            continue;
        }
        assert_eq!(job["state"], "success", "{name}: {log}");
        let greeting = if name.contains("bonjour") { "bonjour" } else { "hello" };
        assert!(log.contains("##[group]Run actions/checkout@v4"), "{log}");
        assert!(log.contains("README says: hello from flux"), "checkout fetched the commit: {log}");
        assert!(log.contains(&format!("##[group]Run echo \"got from-x-{greeting} and env $ADDED")), "{log}");
        assert!(log.contains(&format!("\ngot from-x-{greeting} and env added on")), "{log}");
        assert!(log.contains("tool ran"), "GITHUB_PATH: {log}");
        assert!(log.contains("secret is ***"), "{log}");
        assert!(!log.contains("hunter2-secret"), "secrets are masked: {log}");
        assert!(log.contains("Process completed with exit code 3."), "{log}");
        assert!(log.contains("continue-on-error"), "{log}");
        assert_eq!(log.contains("\nflaky failed as expected"), greeting == "hello", "{log}");
        assert!(log.contains("actions/setup-node@v4 skipped"), "{log}");
        assert!(!log.contains("\nshould not run"), "{log}");
        assert!(log.contains("Skipping step: echo \"should not run\""), "{log}");
        assert!(log.contains("job status is success, conclusion of flaky is success"), "{log}");
        assert!(log.contains("Job finished: success"), "{log}");
    }
    h.assert_cleaned_up();
}

const FAILING: &str = r#"name: CI
on: [push]
jobs:
  broken:
    runs-on: [self-hosted, flux-test]
    steps:
      - run: echo before
      - run: exit 7
      - run: echo "unreachable"
      - if: failure()
        run: echo "cleanup after ${{ job.status }}"
      - if: always()
        timeout-minutes: 1
        run: echo "always runs"
  notify:
    needs: broken
    if: failure()
    runs-on: [self-hosted, flux-test]
    steps:
      - run: echo "notified about the failure"
"#;

#[tokio::test(flavor = "multi_thread")]
async fn failing_jobs_report_failure() {
    let h = Harness::start().await;
    let run_id = h.commit(FAILING).await;
    h.run_runner().await;
    let (_, st) = h.api.get("repos/app/commits/main/status?wait=5").await;
    assert_eq!(st["state"], "failure", "{st}");
    let (run, jobs) = h.jobs(run_id).await;
    assert_eq!(run["state"], "failure", "{run}");
    let (job, log) = jobs.iter().find(|(j, _)| j["name"] == "broken").unwrap();
    assert_eq!(job["state"], "failure", "{log}");
    assert_eq!(job["exit_code"], 7, "{job}");
    assert!(log.contains("\nbefore\n"), "{log}");
    assert!(!log.contains("\nunreachable"), "{log}");
    assert!(log.contains("\ncleanup after failure"), "{log}");
    assert!(log.contains("\nalways runs"), "{log}");
    // `if: failure()` on a job sees the needed job's failure.
    let (job, log) = jobs.iter().find(|(j, _)| j["name"] == "notify").unwrap();
    assert_eq!(job["state"], "success", "{log}");
    assert!(log.contains("\nnotified about the failure"), "{log}");
    h.assert_cleaned_up();
}

const SLOW: &str = r#"name: CI
on: [push]
jobs:
  slow:
    runs-on: [self-hosted, flux-test]
    steps:
      - run: echo started && sleep 120 && echo finished
"#;

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_run_kills_the_job() {
    let h = Harness::start().await;
    let run_id = h.commit(SLOW).await;
    let mut runner = h.spawn_runner();
    h.wait_for_log(run_id, "\nstarted").await;
    let started = std::time::Instant::now();
    let (s, v) = h.api.post(&format!("repos/app/runs/{run_id}/cancel"), json!({})).await;
    assert_eq!(s, 204, "{v}");
    Harness::wait_runner(&mut runner, Duration::from_secs(60)).await;
    assert!(started.elapsed() < Duration::from_secs(30), "{:?}", started.elapsed());
    let (run, jobs) = h.jobs(run_id).await;
    assert_eq!(run["state"], "cancelled", "{run}");
    assert!(!jobs[0].1.contains("\nfinished"), "{}", jobs[0].1);
    h.assert_cleaned_up();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_signal_cancels_running_jobs() {
    let h = Harness::start().await;
    let run_id = h.commit(SLOW).await;
    let mut runner = h.spawn_runner();
    h.wait_for_log(run_id, "\nstarted").await;
    let pid = runner.id().unwrap() as libc::pid_t;
    // SAFETY: signalling our own child process.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(runner.try_wait().unwrap().is_none(), "the first signal lets the job finish");
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    Harness::wait_runner(&mut runner, Duration::from_secs(60)).await;
    let (run, jobs) = h.jobs(run_id).await;
    assert_eq!(jobs[0].0["state"], "cancelled", "{run}");
    assert!(jobs[0].1.contains("The operation was cancelled"), "{}", jobs[0].1);
    h.assert_cleaned_up();
}

/// Needs docker; the image defaults to `alpine:3.20` (`FLUX_TEST_IMAGE`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker"]
async fn container_jobs_and_docker_actions() {
    let image = std::env::var("FLUX_TEST_IMAGE").unwrap_or_else(|_| "alpine:3.20".into());
    let workflow = format!(
        r#"name: CI
on: [push]
jobs:
  boxed:
    runs-on: [self-hosted, flux-test]
    container: {image}
    steps:
      - uses: actions/checkout@v4
      - id: inside
        run: |
          echo "in $(pwd) as $(id -u): $(cat README.md)"
          test -f /.dockerenv && echo "inside docker"
          echo "where=container" >> "$GITHUB_OUTPUT"
          mkdir -p sub && echo built > sub/out.txt
      - working-directory: sub
        run: echo "sub has $(cat out.txt), output ${{{{ steps.inside.outputs.where }}}}"
      - id: action
        uses: docker://{image}
        with:
          entrypoint: sh
          args: -c "echo action sees $INPUT_GREETING in $(pwd); ls; echo from=action >> $GITHUB_OUTPUT"
          greeting: hi
      - run: echo "action said ${{{{ steps.action.outputs.from }}}}"
"#
    );
    let h = Harness::start().await;
    let run_id = h.commit(&workflow).await;
    h.run_runner().await;
    let (run, jobs) = h.jobs(run_id).await;
    let (_, log) = &jobs[0];
    assert_eq!(run["state"], "success", "{log}");
    assert!(log.contains("in /__w as 0: hello from flux"), "{log}");
    assert!(log.contains("inside docker"), "{log}");
    assert!(log.contains("sub has built, output container"), "{log}");
    assert!(log.contains("action sees hi in /github/workspace"), "{log}");
    assert!(log.contains("action said action"), "{log}");
    h.assert_cleaned_up();
}
