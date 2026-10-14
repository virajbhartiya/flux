CREATE TABLE IF NOT EXISTS repos (
  repo_id    BIGSERIAL PRIMARY KEY,
  name       TEXT NOT NULL UNIQUE,
  config     TEXT NOT NULL DEFAULT '{}',
  created_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS changesets (
  repo_id    BIGINT NOT NULL,
  cs_id      BYTEA NOT NULL,
  gen        BIGINT NOT NULL,
  log_seq    BIGINT NOT NULL,
  created_at BIGINT NOT NULL,
  body       BYTEA NOT NULL,
  PRIMARY KEY (repo_id, cs_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS changesets_log ON changesets (repo_id, log_seq);

CREATE TABLE IF NOT EXISTS changeset_parents (
  repo_id BIGINT NOT NULL,
  parent  BYTEA NOT NULL,
  child   BYTEA NOT NULL,
  idx     INT NOT NULL,
  PRIMARY KEY (repo_id, parent, child)
);

CREATE TABLE IF NOT EXISTS refs (
  repo_id BIGINT NOT NULL,
  name    BYTEA NOT NULL,
  cs_id   BYTEA NOT NULL,
  version BIGINT NOT NULL,
  PRIMARY KEY (repo_id, name)
);

CREATE TABLE IF NOT EXISTS ref_log (
  seq     BIGSERIAL PRIMARY KEY,
  repo_id BIGINT NOT NULL,
  name    BYTEA NOT NULL,
  old_cs  BYTEA,
  new_cs  BYTEA,
  actor   TEXT NOT NULL,
  reason  TEXT NOT NULL,
  ts      BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS ref_log_name ON ref_log (repo_id, name, seq);

CREATE TABLE IF NOT EXISTS derived (
  repo_id   BIGINT NOT NULL,
  type_id   INT NOT NULL,
  cs_id     BYTEA NOT NULL,
  value_ref BYTEA NOT NULL,
  PRIMARY KEY (repo_id, type_id, cs_id)
);

CREATE TABLE IF NOT EXISTS content_meta (
  content_id BYTEA PRIMARY KEY,
  size       BIGINT NOT NULL,
  sha1       BYTEA NOT NULL,
  sha256     BYTEA NOT NULL,
  is_binary  BOOLEAN NOT NULL
);

CREATE TABLE IF NOT EXISTS git_map (
  repo_id  BIGINT NOT NULL,
  cs_id    BYTEA NOT NULL,
  git_sha1 BYTEA NOT NULL,
  PRIMARY KEY (repo_id, cs_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS git_map_rev ON git_map (repo_id, git_sha1);
