# flux-runner

The job runner for flux's built-in CI. It claims jobs from a flux server,
runs them on the machine it runs on (optionally inside a container), streams
their logs back and reports the result as a commit status
(`<workflow> / <job>`), which branch protection can require.

Workflows live in `.flux-ci/workflows/*.yml` (or `.github/workflows/*.yml`)
and use a subset of GitHub Actions syntax; see below.

## Install

```sh
cargo install --path crates/flux-runner     # or: cargo build --release -p flux-runner
```

The container image built from the repository's `Dockerfile` also contains
`flux-runner` (`docker run --entrypoint flux-runner …`).

Jobs run with the runner's own user and tools: install `git` (for
`actions/checkout`), `bash`, your toolchains, and `docker` if jobs use
`container:` or `docker://` actions.

## Register and run

An admin creates a runner token once:

```sh
curl -X POST https://flux.example.com/api/v1/runners \
  -H "Authorization: Bearer $FLUX_ADMIN_TOKEN" \
  -d '{"name": "build-1", "labels": ["gpu"]}'
# → {"id": 1, "name": "build-1", "token": "flxr_…"}
```

Then start the runner (prefer the environment variable to the flag, so the
token does not show up in process listings):

```sh
export FLUX_SERVER=https://flux.example.com
export FLUX_RUNNER_TOKEN=flxr_…
flux-runner --labels gpu --concurrency 2
```

| Option | Env | Default | |
|---|---|---|---|
| `--server URL` | `FLUX_SERVER` | | The flux server. |
| `--token flxr_…` | `FLUX_RUNNER_TOKEN` | | Runner token. |
| `--labels a,b` | | | Extra labels, added to the defaults. |
| `--no-default-labels` | | | Use only `--labels`. |
| `--work-dir DIR` | `FLUX_RUNNER_WORK_DIR` | `$TMPDIR/flux-runner` | Where job directories are created. |
| `--concurrency N` | | `1` | Jobs run in parallel. |
| `--once` | | | Exit once no job is waiting, instead of polling forever. |
| `--wait SECS` | | `25` | Long-poll duration of each claim (max 55). |
| `--name NAME` | | host name | Shown in job logs. |

`RUST_LOG` controls the runner's own logging (default `info`).

The first SIGINT/SIGTERM stops claiming new jobs and waits for running ones;
a second one cancels them (they are reported as `cancelled`). Server errors
are retried with exponential backoff; an unknown or revoked token stops the
runner.

### Labels

A job runs on a runner that has every label in its `runs-on`. Default labels:

* `self-hosted`
* the OS: `linux`, `macos` or `windows`
* the architecture: `x64`, `arm64` (or `x86`, `arm`)
* `ubuntu-latest` on Linux, `macos-latest` on macOS, `windows-latest` on Windows

so `runs-on: ubuntu-latest` and `runs-on: [self-hosted, linux, x64]` both
work unchanged on a Linux x64 runner.

## How a job runs

Each job gets a fresh directory under `--work-dir` (`workspace/` is
`GITHUB_WORKSPACE`, `temp/` is `RUNNER_TEMP`), removed when the job ends,
together with any processes it left behind.

* The job's `if` is evaluated first; if false, the job is reported `skipped`.
* Steps run in order. A step's default condition is `success()`; a failed
  step fails the job unless it has `continue-on-error`. The job's exit code is
  the first failing step's.
* `timeout-minutes` (job and step) kill the step's whole process tree.
* Cancelling the run on the server kills the running step within seconds and
  reports `cancelled`.
* Logs are streamed about once a second (or every 64 KiB). Every secret value
  (and anything registered with `::add-mask::`) is replaced by `***` before
  it leaves the machine, even when split across writes.

## Supported workflow subset

The server parses workflows, expands `strategy.matrix` and resolves `needs`;
the runner handles everything inside a job.

**Jobs:** `runs-on`, `if`, `env`, `timeout-minutes`, `continue-on-error`,
`defaults.run.shell` / `working-directory`, `container` (image name or
`container.image`; steps run with `docker exec` in a long-lived container with
the workspace at `/__w`).

**Steps:** `run`, `uses`, `with`, `env`, `if`, `id`, `name`, `shell`,
`working-directory`, `continue-on-error`, `timeout-minutes`.

**Shells:** `bash` (`bash --noprofile --norc -eo pipefail {0}`, the default on
Linux and macOS, falling back to `sh -e {0}`), `sh`, `python`, `pwsh` and
`powershell` (the Windows default), `cmd`, or a custom template such as
`perl {0}`.

**Expressions** (`${{ … }}` in `run`, `env`, `with`, `name`, `container`,
`working-directory`; `if` may omit the braces):

* contexts: `github.*` (`repository`, `ref`, `ref_name`, `sha`, `changeset`,
  `event_name`, `actor`, `server_url`, `workflow`, `job`, `run_id`,
  `workspace`, `token`), `env.*`, `matrix.*`, `secrets.*` (plus
  `secrets.GITHUB_TOKEN`, the job's read-only repository token),
  `steps.<id>.outputs.*`, `steps.<id>.outcome` / `.conclusion`, `runner.os`,
  `runner.arch`, `runner.temp`, `job.status`;
* operators `==`, `!=`, `<`, `<=`, `>`, `>=`, `&&`, `||`, `!`, parentheses,
  `a.b`, `a['b']`, `a.*.b`, and string (`'it''s'`), number, boolean and `null`
  literals, with GitHub's coercion rules (case-insensitive string equality);
* functions `success()`, `failure()`, `always()`, `cancelled()`,
  `contains()`, `startsWith()`, `endsWith()`, `format()`, `join()`,
  `toJSON()`, `fromJSON()`.

**Files and commands:** `GITHUB_OUTPUT` and `GITHUB_ENV` (`name=value` and
`name<<DELIMITER` heredocs), `GITHUB_PATH`, `GITHUB_STEP_SUMMARY` (printed to
the log); `::add-mask::`, `::group::` / `::endgroup::`, `::debug::`,
`::notice::`, `::warning::`, `::error::`, `::stop-commands::` and the legacy
`::set-output`.

**Environment:** the runner's environment (minus its own token), then
`CI=true`, `GITHUB_WORKSPACE`, `GITHUB_SHA`, `GITHUB_REF`, `GITHUB_REF_NAME`,
`GITHUB_REF_TYPE`, `GITHUB_REPOSITORY`, `GITHUB_EVENT_NAME`, `GITHUB_ACTOR`,
`GITHUB_SERVER_URL`, `GITHUB_RUN_ID`, `GITHUB_JOB`, `GITHUB_WORKFLOW`,
`RUNNER_OS`, `RUNNER_ARCH`, `RUNNER_NAME`, `RUNNER_TEMP`, the file command
paths, `FLUX_CI`, `FLUX_SERVER_URL`, `FLUX_REPOSITORY`, `FLUX_CHANGESET`,
`FLUX_JOB_ID`, `FLUX_RUN_ID`; then the job's `env`, values from
`GITHUB_ENV`, and the step's `env`.

### Actions

JavaScript and composite actions are not run. Instead:

| `uses:` | Handling |
|---|---|
| `actions/checkout@*` | Native: fetches the job's commit with the job token and checks it out detached. Supports `path`, `fetch-depth` (default 1; falls back to a full fetch when the server cannot serve shallow clones), `ref`, `repository` (another repository on the same server), `token`, `clean`, `persist-credentials`. |
| `docker://image` | Runs the image with the workspace at `/github/workspace`, `with` values as `INPUT_*`, `with.args` and `with.entrypoint`. Needs docker. |
| `actions/setup-*`, `actions/cache*`, `actions/upload-artifact*`, `actions/download-artifact*` | Skipped with a note; use toolchains installed on the runner. |
| anything else | Skipped with a warning (the step's outcome is `skipped`, not a failure). |

Handlers live in `src/actions.rs`.

### Not supported

`services:`, `hashFiles()`, reusable workflows, artifacts and caches, the
`needs` context (job-level `failure()` / `cancelled()` conditions are checked
against the needed jobs' results on the server), `submodules` and `lfs` in
checkout, and running JavaScript or composite actions.

## Development

```sh
cargo test -p flux-runner                    # unit tests and an end-to-end run against an in-process server
cargo test -p flux-runner -- --ignored       # container jobs and docker:// actions (needs docker)
```
