-- A document is fetched again with If-None-Match / If-Modified-Since like a page, so an
-- unchanged PDF costs a 304 and not its whole body (T-3052).
ALTER TABLE documents ADD COLUMN etag text, ADD COLUMN last_modified text;
