-- T-4659, AG-120: the stored validators go, once, so the next crawl fetches every page and
-- document whole and reads what it states of itself; after that a crawl asks conditionally again.
UPDATE pages SET etag = NULL, last_modified = NULL;
UPDATE documents SET etag = NULL, last_modified = NULL;
