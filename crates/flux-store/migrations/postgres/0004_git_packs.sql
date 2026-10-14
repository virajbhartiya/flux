-- Cached full-history git pack per repository (served to clones, with a
-- freshly generated tail for anything newer than `tips`).
CREATE TABLE IF NOT EXISTS git_packs (
  repo_id    BIGINT PRIMARY KEY,
  content_id BYTEA NOT NULL,
  objects    BIGINT NOT NULL,
  tips       BYTEA NOT NULL,
  created_ms BIGINT NOT NULL
);
