-- Cached full-history git pack per repository (served to clones, with a
-- freshly generated tail for anything newer than `tips`).
CREATE TABLE IF NOT EXISTS git_packs (
  repo_id    INTEGER PRIMARY KEY,
  content_id BLOB NOT NULL,
  objects    INTEGER NOT NULL,
  tips       BLOB NOT NULL,
  created_ms INTEGER NOT NULL
);
