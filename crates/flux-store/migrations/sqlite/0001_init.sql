CREATE TABLE IF NOT EXISTS repos (
  repo_id    INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT NOT NULL UNIQUE,
  config     TEXT NOT NULL DEFAULT '{}',
  created_at INTEGER NOT NULL
);

-- A rowid table: bodies can be large, which WITHOUT ROWID handles badly.
-- The body column is last so lookups of gen/log_seq never touch overflow pages.
CREATE TABLE IF NOT EXISTS changesets (
  repo_id    INTEGER NOT NULL,
  cs_id      BLOB NOT NULL,
  gen        INTEGER NOT NULL,
  log_seq    INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  body       BLOB NOT NULL,
  UNIQUE (repo_id, cs_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS changesets_log ON changesets (repo_id, log_seq);

CREATE TABLE IF NOT EXISTS changeset_parents (
  repo_id INTEGER NOT NULL,
  parent  BLOB NOT NULL,
  child   BLOB NOT NULL,
  idx     INTEGER NOT NULL,
  PRIMARY KEY (repo_id, parent, child)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS refs (
  repo_id INTEGER NOT NULL,
  name    BLOB NOT NULL,
  cs_id   BLOB NOT NULL,
  version INTEGER NOT NULL,
  PRIMARY KEY (repo_id, name)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS ref_log (
  seq     INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_id INTEGER NOT NULL,
  name    BLOB NOT NULL,
  old_cs  BLOB,
  new_cs  BLOB,
  actor   TEXT NOT NULL,
  reason  TEXT NOT NULL,
  ts      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS ref_log_name ON ref_log (repo_id, name, seq);

CREATE TABLE IF NOT EXISTS derived (
  repo_id   INTEGER NOT NULL,
  type_id   INTEGER NOT NULL,
  cs_id     BLOB NOT NULL,
  value_ref BLOB NOT NULL,
  PRIMARY KEY (repo_id, type_id, cs_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS content_meta (
  content_id BLOB PRIMARY KEY,
  size       INTEGER NOT NULL,
  sha1       BLOB NOT NULL,
  sha256     BLOB NOT NULL,
  is_binary  INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS git_map (
  repo_id  INTEGER NOT NULL,
  cs_id    BLOB NOT NULL,
  git_sha1 BLOB NOT NULL,
  PRIMARY KEY (repo_id, cs_id)
) WITHOUT ROWID;
CREATE UNIQUE INDEX IF NOT EXISTS git_map_rev ON git_map (repo_id, git_sha1);
