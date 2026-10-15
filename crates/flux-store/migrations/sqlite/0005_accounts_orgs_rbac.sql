-- Accounts, organizations, teams and repository roles.

ALTER TABLE users ADD COLUMN email TEXT;
ALTER TABLE users ADD COLUMN display_name TEXT NOT NULL DEFAULT '';
ALTER TABLE users ADD COLUMN password_hash TEXT;
ALTER TABLE users ADD COLUMN disabled INTEGER NOT NULL DEFAULT 0;
CREATE UNIQUE INDEX IF NOT EXISTS users_email ON users (email) WHERE email IS NOT NULL;

-- Tokens: API tokens and web sessions (kind), optional expiry and scopes.
ALTER TABLE tokens ADD COLUMN kind TEXT NOT NULL DEFAULT 'api';
ALTER TABLE tokens ADD COLUMN scopes TEXT NOT NULL DEFAULT 'all';
ALTER TABLE tokens ADD COLUMN expires_at INTEGER;
ALTER TABLE tokens ADD COLUMN last_used INTEGER;
CREATE INDEX IF NOT EXISTS tokens_user ON tokens (user_id);

CREATE TABLE IF NOT EXISTS orgs (
  org_id       INTEGER PRIMARY KEY AUTOINCREMENT,
  name         TEXT NOT NULL UNIQUE,
  display_name TEXT NOT NULL DEFAULT '',
  description  TEXT NOT NULL DEFAULT '',
  -- Role every member gets on the org's repositories: none | read | write.
  base_role    TEXT NOT NULL DEFAULT 'read',
  created_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS org_members (
  org_id  INTEGER NOT NULL,
  user_id INTEGER NOT NULL,
  role    TEXT NOT NULL, -- owner | member
  PRIMARY KEY (org_id, user_id)
);
CREATE INDEX IF NOT EXISTS org_members_user ON org_members (user_id);

CREATE TABLE IF NOT EXISTS teams (
  team_id     INTEGER PRIMARY KEY AUTOINCREMENT,
  org_id      INTEGER NOT NULL,
  name        TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  created_at  INTEGER NOT NULL,
  UNIQUE (org_id, name)
);

CREATE TABLE IF NOT EXISTS team_members (
  team_id INTEGER NOT NULL,
  user_id INTEGER NOT NULL,
  PRIMARY KEY (team_id, user_id)
);
CREATE INDEX IF NOT EXISTS team_members_user ON team_members (user_id);

-- Role grants on a repository to a user (collaborator) or a team.
CREATE TABLE IF NOT EXISTS repo_grants (
  repo_id    INTEGER NOT NULL,
  kind       TEXT NOT NULL, -- user | team
  subject_id INTEGER NOT NULL,
  role       TEXT NOT NULL, -- read | triage | write | maintain | admin
  PRIMARY KEY (repo_id, kind, subject_id)
);

-- Ownership and visibility. Existing repositories keep their behaviour:
-- public, and writable by any signed-in user.
ALTER TABLE repos ADD COLUMN owner_kind TEXT NOT NULL DEFAULT '';
ALTER TABLE repos ADD COLUMN owner_id INTEGER NOT NULL DEFAULT 0;
ALTER TABLE repos ADD COLUMN visibility TEXT NOT NULL DEFAULT 'public';
ALTER TABLE repos ADD COLUMN signed_in_role TEXT NOT NULL DEFAULT 'write';

CREATE TABLE IF NOT EXISTS audit_log (
  seq    INTEGER PRIMARY KEY AUTOINCREMENT,
  ts     INTEGER NOT NULL,
  actor  TEXT NOT NULL,
  action TEXT NOT NULL,
  target TEXT NOT NULL,
  detail TEXT NOT NULL DEFAULT ''
);
