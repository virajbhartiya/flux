-- Issues, labels, milestones, releases, checks, webhooks, SSH keys,
-- notifications, external identities, two-factor auth, LFS, CI and secrets.

-- Accounts: kind (user | bot | placeholder) and two-factor auth.
ALTER TABLE users ADD COLUMN kind TEXT NOT NULL DEFAULT 'user';
ALTER TABLE users ADD COLUMN totp_secret TEXT;
-- Space-separated SHA-256 hex hashes of unused recovery codes.
ALTER TABLE users ADD COLUMN recovery_codes TEXT NOT NULL DEFAULT '';

-- Reviews: drafts, and the repository a forked review comes from.
ALTER TABLE reviews ADD COLUMN draft INTEGER NOT NULL DEFAULT 0;
ALTER TABLE reviews ADD COLUMN source_repo INTEGER NOT NULL DEFAULT 0;
ALTER TABLE reviews ADD COLUMN milestone INTEGER;

ALTER TABLE repos ADD COLUMN fork_of INTEGER;

-- Issues share the number space with reviews (like GitHub issues and PRs).
CREATE TABLE IF NOT EXISTS issues (
  repo_id      INTEGER NOT NULL,
  number       INTEGER NOT NULL,
  title        TEXT NOT NULL,
  body         TEXT NOT NULL,
  author       TEXT NOT NULL,
  state        TEXT NOT NULL,          -- open | closed
  state_reason TEXT NOT NULL DEFAULT '', -- completed | not_planned | duplicate
  milestone    INTEGER,
  locked       INTEGER NOT NULL DEFAULT 0,
  comments     INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL,
  closed_at    INTEGER,
  PRIMARY KEY (repo_id, number)
);
CREATE INDEX IF NOT EXISTS issues_state ON issues (repo_id, state, number);
CREATE INDEX IF NOT EXISTS issues_updated ON issues (repo_id, updated_at);

-- Issue timeline: comments and events (closed, labeled, assigned, ...).
CREATE TABLE IF NOT EXISTS issue_events (
  seq        INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_id    INTEGER NOT NULL,
  number     INTEGER NOT NULL,
  kind       TEXT NOT NULL,
  author     TEXT NOT NULL,
  body       TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  external_id TEXT
);
CREATE INDEX IF NOT EXISTS issue_events_item ON issue_events (repo_id, number, seq);
CREATE UNIQUE INDEX IF NOT EXISTS issue_events_external ON issue_events (repo_id, external_id) WHERE external_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS labels (
  repo_id     INTEGER NOT NULL,
  name        TEXT NOT NULL,
  color       TEXT NOT NULL DEFAULT '8a8f98',
  description TEXT NOT NULL DEFAULT '',
  PRIMARY KEY (repo_id, name)
);

-- Labels on issues and reviews (one number space).
CREATE TABLE IF NOT EXISTS item_labels (
  repo_id INTEGER NOT NULL,
  number  INTEGER NOT NULL,
  label   TEXT NOT NULL,
  PRIMARY KEY (repo_id, number, label)
);
CREATE INDEX IF NOT EXISTS item_labels_label ON item_labels (repo_id, label);

-- Assignees and requested reviewers.
CREATE TABLE IF NOT EXISTS item_people (
  repo_id INTEGER NOT NULL,
  number  INTEGER NOT NULL,
  kind    TEXT NOT NULL, -- assignee | reviewer
  name    TEXT NOT NULL,
  PRIMARY KEY (repo_id, number, kind, name)
);
CREATE INDEX IF NOT EXISTS item_people_name ON item_people (name, kind);

CREATE TABLE IF NOT EXISTS milestones (
  repo_id     INTEGER NOT NULL,
  number      INTEGER NOT NULL,
  title       TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  state       TEXT NOT NULL DEFAULT 'open',
  due_at      INTEGER,
  created_at  INTEGER NOT NULL,
  closed_at   INTEGER,
  PRIMARY KEY (repo_id, number)
);

CREATE TABLE IF NOT EXISTS releases (
  release_id   INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_id      INTEGER NOT NULL,
  tag          TEXT NOT NULL,
  name         TEXT NOT NULL,
  body         TEXT NOT NULL,
  draft        INTEGER NOT NULL DEFAULT 0,
  prerelease   INTEGER NOT NULL DEFAULT 0,
  author       TEXT NOT NULL,
  target       BLOB,
  created_at   INTEGER NOT NULL,
  published_at INTEGER,
  UNIQUE (repo_id, tag)
);

CREATE TABLE IF NOT EXISTS release_assets (
  asset_id     INTEGER PRIMARY KEY AUTOINCREMENT,
  release_id   INTEGER NOT NULL,
  name         TEXT NOT NULL,
  content_id   BLOB NOT NULL,
  size         INTEGER NOT NULL,
  content_type TEXT NOT NULL,
  downloads    INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL,
  UNIQUE (release_id, name)
);

-- Commit statuses: the latest state per (changeset, context).
CREATE TABLE IF NOT EXISTS statuses (
  repo_id     INTEGER NOT NULL,
  cs_id       BLOB NOT NULL,
  context     TEXT NOT NULL,
  state       TEXT NOT NULL, -- pending | success | failure | error
  description TEXT NOT NULL DEFAULT '',
  target_url  TEXT NOT NULL DEFAULT '',
  creator     TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  PRIMARY KEY (repo_id, cs_id, context)
);

CREATE TABLE IF NOT EXISTS webhooks (
  hook_id      INTEGER PRIMARY KEY AUTOINCREMENT,
  scope_kind   TEXT NOT NULL, -- repo | org
  scope_id     INTEGER NOT NULL,
  url          TEXT NOT NULL,
  secret       TEXT NOT NULL DEFAULT '',
  events       TEXT NOT NULL DEFAULT 'push',
  active       INTEGER NOT NULL DEFAULT 1,
  created_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS webhooks_scope ON webhooks (scope_kind, scope_id);

-- Durable delivery queue with retries.
CREATE TABLE IF NOT EXISTS webhook_deliveries (
  delivery_id INTEGER PRIMARY KEY AUTOINCREMENT,
  hook_id     INTEGER NOT NULL,
  event       TEXT NOT NULL,
  guid        TEXT NOT NULL,
  payload     TEXT NOT NULL,
  state       TEXT NOT NULL DEFAULT 'pending', -- pending | ok | failed
  attempts    INTEGER NOT NULL DEFAULT 0,
  next_at     INTEGER NOT NULL,
  status_code INTEGER NOT NULL DEFAULT 0,
  response    TEXT NOT NULL DEFAULT '',
  duration_ms INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS webhook_deliveries_due ON webhook_deliveries (state, next_at);
CREATE INDEX IF NOT EXISTS webhook_deliveries_hook ON webhook_deliveries (hook_id, delivery_id);

CREATE TABLE IF NOT EXISTS ssh_keys (
  key_id      INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id     INTEGER NOT NULL,
  title       TEXT NOT NULL,
  fingerprint TEXT NOT NULL UNIQUE,
  public_key  TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  last_used   INTEGER
);
CREATE INDEX IF NOT EXISTS ssh_keys_user ON ssh_keys (user_id);

CREATE TABLE IF NOT EXISTS notifications (
  notification_id INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id    INTEGER NOT NULL,
  repo_id    INTEGER NOT NULL,
  number     INTEGER NOT NULL,
  kind       TEXT NOT NULL, -- issue | review | release | ci
  reason     TEXT NOT NULL, -- mention | assigned | review_requested | author | subscribed | ...
  title      TEXT NOT NULL,
  unread     INTEGER NOT NULL DEFAULT 1,
  updated_at INTEGER NOT NULL,
  UNIQUE (user_id, repo_id, number, kind)
);
CREATE INDEX IF NOT EXISTS notifications_user ON notifications (user_id, unread, updated_at);

-- Thread subscriptions (subscribed | ignored) and repository watches.
CREATE TABLE IF NOT EXISTS subscriptions (
  user_id INTEGER NOT NULL,
  repo_id INTEGER NOT NULL,
  number  INTEGER NOT NULL, -- 0: the whole repository
  state   TEXT NOT NULL,
  PRIMARY KEY (user_id, repo_id, number)
);
CREATE INDEX IF NOT EXISTS subscriptions_item ON subscriptions (repo_id, number);

-- Sign-in through external providers (github, google, oidc).
CREATE TABLE IF NOT EXISTS identities (
  provider   TEXT NOT NULL,
  subject    TEXT NOT NULL,
  user_id    INTEGER NOT NULL,
  login      TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  PRIMARY KEY (provider, subject)
);
CREATE INDEX IF NOT EXISTS identities_user ON identities (user_id);

CREATE TABLE IF NOT EXISTS lfs_objects (
  repo_id    INTEGER NOT NULL,
  oid        TEXT NOT NULL,
  size       INTEGER NOT NULL,
  content_id BLOB NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (repo_id, oid)
);

-- CI: runs (one per workflow and trigger), jobs, logs and runners.
CREATE TABLE IF NOT EXISTS ci_runs (
  run_id     INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_id    INTEGER NOT NULL,
  workflow   TEXT NOT NULL,
  event      TEXT NOT NULL,
  ref_name   TEXT NOT NULL,
  cs_id      BLOB NOT NULL,
  review     INTEGER,
  state      TEXT NOT NULL, -- queued | running | success | failure | cancelled
  actor      TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS ci_runs_repo ON ci_runs (repo_id, run_id);

CREATE TABLE IF NOT EXISTS ci_jobs (
  job_id      INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id      INTEGER NOT NULL,
  repo_id     INTEGER NOT NULL,
  name        TEXT NOT NULL,
  needs       TEXT NOT NULL DEFAULT '',
  labels      TEXT NOT NULL DEFAULT '',
  spec        TEXT NOT NULL,
  state       TEXT NOT NULL, -- blocked | queued | running | success | failure | cancelled | skipped
  runner      TEXT NOT NULL DEFAULT '',
  lease_until INTEGER NOT NULL DEFAULT 0,
  started_at  INTEGER,
  finished_at INTEGER,
  exit_code   INTEGER
);
CREATE INDEX IF NOT EXISTS ci_jobs_run ON ci_jobs (run_id);
CREATE INDEX IF NOT EXISTS ci_jobs_state ON ci_jobs (state, job_id);

CREATE TABLE IF NOT EXISTS ci_logs (
  job_id INTEGER NOT NULL,
  seq    INTEGER NOT NULL,
  data   BLOB NOT NULL,
  PRIMARY KEY (job_id, seq)
);

CREATE TABLE IF NOT EXISTS ci_runners (
  runner_id  INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT NOT NULL,
  token_hash BLOB NOT NULL UNIQUE,
  labels     TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  last_seen  INTEGER
);

-- Encrypted secrets for CI (scope: repo or org).
CREATE TABLE IF NOT EXISTS secrets (
  scope_kind TEXT NOT NULL,
  scope_id   INTEGER NOT NULL,
  name       TEXT NOT NULL,
  ciphertext BLOB NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (scope_kind, scope_id, name)
);
