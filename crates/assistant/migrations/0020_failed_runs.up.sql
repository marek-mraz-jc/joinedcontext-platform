-- T-4749: a page or document that failed to read, or that a complete crawl no longer reached,
-- in this many runs in a row; the crawl deletes it (and its chunks) at 3, and a read resets it.
ALTER TABLE pages ADD COLUMN failed_runs smallint NOT NULL DEFAULT 0 CHECK (failed_runs >= 0);
ALTER TABLE documents ADD COLUMN failed_runs smallint NOT NULL DEFAULT 0 CHECK (failed_runs >= 0);
