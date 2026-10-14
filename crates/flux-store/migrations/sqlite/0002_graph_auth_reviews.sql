CREATE INDEX IF NOT EXISTS changeset_parents_child ON changeset_parents (repo_id, child);

CREATE TABLE IF NOT EXISTS users (
  user_id    INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT NOT NULL UNIQUE,
  is_admin   INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);

-- Bearer tokens are stored as SHA-256 hashes; the plaintext is shown once.
CREATE TABLE IF NOT EXISTS tokens (
  token_hash BLOB PRIMARY KEY,
  user_id    INTEGER NOT NULL,
  label      TEXT NOT NULL,
  created_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS reviews (
  repo_id    INTEGER NOT NULL,
  number     INTEGER NOT NULL,
  title      TEXT NOT NULL,
  body       TEXT NOT NULL,
  author     TEXT NOT NULL,
  head       BLOB NOT NULL,
  target     BLOB NOT NULL,
  state      TEXT NOT NULL,
  landed     BLOB,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (repo_id, number)
);
CREATE INDEX IF NOT EXISTS reviews_state ON reviews (repo_id, state, number);

CREATE TABLE IF NOT EXISTS review_events (
  seq     INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_id INTEGER NOT NULL,
  number  INTEGER NOT NULL,
  kind    TEXT NOT NULL,
  author  TEXT NOT NULL,
  body    TEXT NOT NULL,
  cs_id   BLOB,
  path    BLOB,
  line    INTEGER,
  ts      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS review_events_by_review ON review_events (repo_id, number, seq);
CREATE INDEX IF NOT EXISTS ref_log_repo ON ref_log (repo_id, seq);
