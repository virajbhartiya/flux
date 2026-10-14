-- git objects generated from flux data (the git smart-HTTP frontend).
-- Tree mappings are global: a flux tree always maps to the same git tree.
CREATE TABLE IF NOT EXISTS git_trees (
  tree_id BYTEA PRIMARY KEY,
  git_sha BYTEA NOT NULL
);
CREATE INDEX IF NOT EXISTS git_trees_sha ON git_trees (git_sha);

CREATE TABLE IF NOT EXISTS git_commits (
  repo_id BIGINT NOT NULL,
  git_sha BYTEA NOT NULL,
  cs_id   BYTEA NOT NULL,
  PRIMARY KEY (repo_id, git_sha)
);
CREATE INDEX IF NOT EXISTS git_commits_cs ON git_commits (repo_id, cs_id);

CREATE INDEX IF NOT EXISTS content_meta_sha1 ON content_meta (sha1);
