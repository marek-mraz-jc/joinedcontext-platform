-- The index status an administrator reads (API/05 §3, T-4197): why a page or a document failed to
-- read (an HTTP status or the network error, never its content) and when a crawl job ended.
ALTER TABLE pages ADD COLUMN error text CHECK (error IS NULL OR char_length(error) <= 300);
ALTER TABLE documents ADD COLUMN error text CHECK (error IS NULL OR char_length(error) <= 300);
ALTER TABLE crawl_jobs ADD COLUMN finished_at timestamptz;
