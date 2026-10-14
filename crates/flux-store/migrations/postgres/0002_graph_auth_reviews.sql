CREATE INDEX IF NOT EXISTS changeset_parents_child ON changeset_parents (repo_id, child);

CREATE TABLE IF NOT EXISTS users (
  user_id    BIGSERIAL PRIMARY KEY,
  name       TEXT NOT NULL UNIQUE,
  is_admin   BOOLEAN NOT NULL DEFAULT FALSE,
  created_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS tokens (
  token_hash BYTEA PRIMARY KEY,
  user_id    BIGINT NOT NULL,
  label      TEXT NOT NULL,
  created_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS reviews (
  repo_id    BIGINT NOT NULL,
  number     BIGINT NOT NULL,
  title      TEXT NOT NULL,
  body       TEXT NOT NULL,
  author     TEXT NOT NULL,
  head       BYTEA NOT NULL,
  target     BYTEA NOT NULL,
  state      TEXT NOT NULL,
  landed     BYTEA,
  created_at BIGINT NOT NULL,
  updated_at BIGINT NOT NULL,
  PRIMARY KEY (repo_id, number)
);
CREATE INDEX IF NOT EXISTS reviews_state ON reviews (repo_id, state, number);

CREATE TABLE IF NOT EXISTS review_events (
  seq     BIGSERIAL PRIMARY KEY,
  repo_id BIGINT NOT NULL,
  number  BIGINT NOT NULL,
  kind    TEXT NOT NULL,
  author  TEXT NOT NULL,
  body    TEXT NOT NULL,
  cs_id   BYTEA,
  path    BYTEA,
  line    BIGINT,
  ts      BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS review_events_by_review ON review_events (repo_id, number, seq);
CREATE INDEX IF NOT EXISTS ref_log_repo ON ref_log (repo_id, seq);
