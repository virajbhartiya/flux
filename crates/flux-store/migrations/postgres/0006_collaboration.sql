-- Issues, labels, milestones, releases, checks, webhooks, SSH keys,
-- notifications, external identities, two-factor auth, LFS, CI and secrets.

-- Accounts: kind (user | bot | placeholder) and two-factor auth.
ALTER TABLE users ADD COLUMN kind TEXT NOT NULL DEFAULT 'user';
ALTER TABLE users ADD COLUMN totp_secret TEXT;
-- Space-separated SHA-256 hex hashes of unused recovery codes.
ALTER TABLE users ADD COLUMN recovery_codes TEXT NOT NULL DEFAULT '';

-- Reviews: drafts, and the repository a forked review comes from.
ALTER TABLE reviews ADD COLUMN draft BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE reviews ADD COLUMN source_repo BIGINT NOT NULL DEFAULT 0;
ALTER TABLE reviews ADD COLUMN milestone BIGINT;

ALTER TABLE repos ADD COLUMN fork_of BIGINT;

-- Issues share the number space with reviews (like GitHub issues and PRs).
CREATE TABLE IF NOT EXISTS issues (
  repo_id      BIGINT NOT NULL,
  number       BIGINT NOT NULL,
  title        TEXT NOT NULL,
  body         TEXT NOT NULL,
  author       TEXT NOT NULL,
  state        TEXT NOT NULL,          -- open | closed
  state_reason TEXT NOT NULL DEFAULT '', -- completed | not_planned | duplicate
  milestone    BIGINT,
  locked       BOOLEAN NOT NULL DEFAULT FALSE,
  comments     BIGINT NOT NULL DEFAULT 0,
  created_at   BIGINT NOT NULL,
  updated_at   BIGINT NOT NULL,
  closed_at    BIGINT,
  PRIMARY KEY (repo_id, number)
);
CREATE INDEX IF NOT EXISTS issues_state ON issues (repo_id, state, number);
CREATE INDEX IF NOT EXISTS issues_updated ON issues (repo_id, updated_at);

-- Issue timeline: comments and events (closed, labeled, assigned, ...).
CREATE TABLE IF NOT EXISTS issue_events (
  seq        BIGSERIAL PRIMARY KEY,
  repo_id    BIGINT NOT NULL,
  number     BIGINT NOT NULL,
  kind       TEXT NOT NULL,
  author     TEXT NOT NULL,
  body       TEXT NOT NULL,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  external_id TEXT
);
CREATE INDEX IF NOT EXISTS issue_events_item ON issue_events (repo_id, number, seq);
CREATE UNIQUE INDEX IF NOT EXISTS issue_events_external ON issue_events (repo_id, external_id) WHERE external_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS labels (
  repo_id     BIGINT NOT NULL,
  name        TEXT NOT NULL,
  color       TEXT NOT NULL DEFAULT '8a8f98',
  description TEXT NOT NULL DEFAULT '',
  PRIMARY KEY (repo_id, name)
);

-- Labels on issues and reviews (one number space).
CREATE TABLE IF NOT EXISTS item_labels (
  repo_id BIGINT NOT NULL,
  number  BIGINT NOT NULL,
  label   TEXT NOT NULL,
  PRIMARY KEY (repo_id, number, label)
);
CREATE INDEX IF NOT EXISTS item_labels_label ON item_labels (repo_id, label);

-- Assignees and requested reviewers.
CREATE TABLE IF NOT EXISTS item_people (
  repo_id BIGINT NOT NULL,
  number  BIGINT NOT NULL,
  kind    TEXT NOT NULL, -- assignee | reviewer
  name    TEXT NOT NULL,
  PRIMARY KEY (repo_id, number, kind, name)
);
CREATE INDEX IF NOT EXISTS item_people_name ON item_people (name, kind);

CREATE TABLE IF NOT EXISTS milestones (
  repo_id     BIGINT NOT NULL,
  number      BIGINT NOT NULL,
  title       TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  state       TEXT NOT NULL DEFAULT 'open',
  due_at      BIGINT,
  created_at  BIGINT NOT NULL,
  closed_at   BIGINT,
  PRIMARY KEY (repo_id, number)
);

CREATE TABLE IF NOT EXISTS releases (
  release_id   BIGSERIAL PRIMARY KEY,
  repo_id      BIGINT NOT NULL,
  tag          TEXT NOT NULL,
  name         TEXT NOT NULL,
  body         TEXT NOT NULL,
  draft        BOOLEAN NOT NULL DEFAULT FALSE,
  prerelease   BOOLEAN NOT NULL DEFAULT FALSE,
  author       TEXT NOT NULL,
  target       BYTEA,
  created_at   BIGINT NOT NULL,
  published_at BIGINT,
  UNIQUE (repo_id, tag)
);

CREATE TABLE IF NOT EXISTS release_assets (
  asset_id     BIGSERIAL PRIMARY KEY,
  release_id   BIGINT NOT NULL,
  name         TEXT NOT NULL,
  content_id   BYTEA NOT NULL,
  size         BIGINT NOT NULL,
  content_type TEXT NOT NULL,
  downloads    BIGINT NOT NULL DEFAULT 0,
  created_at   BIGINT NOT NULL,
  UNIQUE (release_id, name)
);

-- Commit statuses: the latest state per (changeset, context).
CREATE TABLE IF NOT EXISTS statuses (
  repo_id     BIGINT NOT NULL,
  cs_id       BYTEA NOT NULL,
  context     TEXT NOT NULL,
  state       TEXT NOT NULL, -- pending | success | failure | error
  description TEXT NOT NULL DEFAULT '',
  target_url  TEXT NOT NULL DEFAULT '',
  creator     TEXT NOT NULL,
  created_at  BIGINT NOT NULL,
  updated_at  BIGINT NOT NULL,
  PRIMARY KEY (repo_id, cs_id, context)
);

CREATE TABLE IF NOT EXISTS webhooks (
  hook_id      BIGSERIAL PRIMARY KEY,
  scope_kind   TEXT NOT NULL, -- repo | org
  scope_id     BIGINT NOT NULL,
  url          TEXT NOT NULL,
  secret       TEXT NOT NULL DEFAULT '',
  events       TEXT NOT NULL DEFAULT 'push',
  active       BOOLEAN NOT NULL DEFAULT TRUE,
  created_at   BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS webhooks_scope ON webhooks (scope_kind, scope_id);

-- Durable delivery queue with retries.
CREATE TABLE IF NOT EXISTS webhook_deliveries (
  delivery_id BIGSERIAL PRIMARY KEY,
  hook_id     BIGINT NOT NULL,
  event       TEXT NOT NULL,
  guid        TEXT NOT NULL,
  payload     TEXT NOT NULL,
  state       TEXT NOT NULL DEFAULT 'pending', -- pending | ok | failed
  attempts    BIGINT NOT NULL DEFAULT 0,
  next_at     BIGINT NOT NULL,
  status_code BIGINT NOT NULL DEFAULT 0,
  response    TEXT NOT NULL DEFAULT '',
  duration_ms BIGINT NOT NULL DEFAULT 0,
  created_at  BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS webhook_deliveries_due ON webhook_deliveries (state, next_at);
CREATE INDEX IF NOT EXISTS webhook_deliveries_hook ON webhook_deliveries (hook_id, delivery_id);

CREATE TABLE IF NOT EXISTS ssh_keys (
  key_id      BIGSERIAL PRIMARY KEY,
  user_id     BIGINT NOT NULL,
  title       TEXT NOT NULL,
  fingerprint TEXT NOT NULL UNIQUE,
  public_key  TEXT NOT NULL,
  created_at  BIGINT NOT NULL,
  last_used   BIGINT
);
CREATE INDEX IF NOT EXISTS ssh_keys_user ON ssh_keys (user_id);

CREATE TABLE IF NOT EXISTS notifications (
  notification_id BIGSERIAL PRIMARY KEY,
  user_id    BIGINT NOT NULL,
  repo_id    BIGINT NOT NULL,
  number     BIGINT NOT NULL,
  kind       TEXT NOT NULL, -- issue | review | release | ci
  reason     TEXT NOT NULL, -- mention | assigned | review_requested | author | subscribed | ...
  title      TEXT NOT NULL,
  unread     BOOLEAN NOT NULL DEFAULT TRUE,
  updated_at BIGINT NOT NULL,
  UNIQUE (user_id, repo_id, number, kind)
);
CREATE INDEX IF NOT EXISTS notifications_user ON notifications (user_id, unread, updated_at);

-- Thread subscriptions (subscribed | ignored) and repository watches.
CREATE TABLE IF NOT EXISTS subscriptions (
  user_id BIGINT NOT NULL,
  repo_id BIGINT NOT NULL,
  number  BIGINT NOT NULL, -- 0: the whole repository
  state   TEXT NOT NULL,
  PRIMARY KEY (user_id, repo_id, number)
);
CREATE INDEX IF NOT EXISTS subscriptions_item ON subscriptions (repo_id, number);

-- Sign-in through external providers (github, google, oidc).
CREATE TABLE IF NOT EXISTS identities (
  provider   TEXT NOT NULL,
  subject    TEXT NOT NULL,
  user_id    BIGINT NOT NULL,
  login      TEXT NOT NULL DEFAULT '',
  created_at BIGINT NOT NULL,
  PRIMARY KEY (provider, subject)
);
CREATE INDEX IF NOT EXISTS identities_user ON identities (user_id);

CREATE TABLE IF NOT EXISTS lfs_objects (
  repo_id    BIGINT NOT NULL,
  oid        TEXT NOT NULL,
  size       BIGINT NOT NULL,
  content_id BYTEA NOT NULL,
  created_at BIGINT NOT NULL,
  PRIMARY KEY (repo_id, oid)
);

-- CI: runs (one per workflow and trigger), jobs, logs and runners.
CREATE TABLE IF NOT EXISTS ci_runs (
  run_id     BIGSERIAL PRIMARY KEY,
  repo_id    BIGINT NOT NULL,
  workflow   TEXT NOT NULL,
  event      TEXT NOT NULL,
  ref_name   TEXT NOT NULL,
  cs_id      BYTEA NOT NULL,
  review     BIGINT,
  state      TEXT NOT NULL, -- queued | running | success | failure | cancelled
  actor      TEXT NOT NULL,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS ci_runs_repo ON ci_runs (repo_id, run_id);

CREATE TABLE IF NOT EXISTS ci_jobs (
  job_id      BIGSERIAL PRIMARY KEY,
  run_id      BIGINT NOT NULL,
  repo_id     BIGINT NOT NULL,
  name        TEXT NOT NULL,
  needs       TEXT NOT NULL DEFAULT '',
  labels      TEXT NOT NULL DEFAULT '',
  spec        TEXT NOT NULL,
  state       TEXT NOT NULL, -- blocked | queued | running | success | failure | cancelled | skipped
  runner      TEXT NOT NULL DEFAULT '',
  lease_until BIGINT NOT NULL DEFAULT 0,
  started_at  BIGINT,
  finished_at BIGINT,
  exit_code   BIGINT
);
CREATE INDEX IF NOT EXISTS ci_jobs_run ON ci_jobs (run_id);
CREATE INDEX IF NOT EXISTS ci_jobs_state ON ci_jobs (state, job_id);

CREATE TABLE IF NOT EXISTS ci_logs (
  job_id BIGINT NOT NULL,
  seq    BIGINT NOT NULL,
  data   BYTEA NOT NULL,
  PRIMARY KEY (job_id, seq)
);

CREATE TABLE IF NOT EXISTS ci_runners (
  runner_id  BIGSERIAL PRIMARY KEY,
  name       TEXT NOT NULL,
  token_hash BYTEA NOT NULL UNIQUE,
  labels     TEXT NOT NULL DEFAULT '',
  created_at BIGINT NOT NULL,
  last_seen  BIGINT
);

-- Encrypted secrets for CI (scope: repo or org).
CREATE TABLE IF NOT EXISTS secrets (
  scope_kind TEXT NOT NULL,
  scope_id   BIGINT NOT NULL,
  name       TEXT NOT NULL,
  ciphertext BYTEA NOT NULL,
  updated_at BIGINT NOT NULL,
  PRIMARY KEY (scope_kind, scope_id, name)
);
