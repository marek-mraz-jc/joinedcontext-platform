-- T-4659, AG-120: what a page or a PDF states of itself, read at crawl time; NULL when it does
-- not state it, never guessed. A search hit for an MCP server carries it.
ALTER TABLE pages
    ADD COLUMN title       text,
    ADD COLUMN description text,
    ADD COLUMN published   timestamptz,
    ADD COLUMN modified    timestamptz;
ALTER TABLE documents
    ADD COLUMN title       text,
    ADD COLUMN description text,
    ADD COLUMN published   timestamptz,
    ADD COLUMN modified    timestamptz;
-- The validators go, once, so the next crawl fetches every page and document whole and reads
-- what it states; after that a crawl asks conditionally again.
UPDATE pages SET etag = NULL, last_modified = NULL;
UPDATE documents SET etag = NULL, last_modified = NULL;
