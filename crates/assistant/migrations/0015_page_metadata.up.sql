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
