-- Imported review timeline entries keep their source ids, so re-imports
-- (mirror syncs) do not duplicate them.
ALTER TABLE review_events ADD COLUMN external_id TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS review_events_external ON review_events (repo_id, external_id) WHERE external_id IS NOT NULL;
