-- T-6547: the embedder claims the chunks still without a vector, `ORDER BY id LIMIT 16`, every
-- round of every project (`embed::embed_missing`). Without an index on that condition each claim
-- read every chunk of every project: on dev 1.2 to 2.4 s a call, 105 slow statements an hour on
-- the Postgres the Portal and the broker share. Same shape as `chunks_terms_pending` (0022): the
-- row-level security condition on `project` is the index's first column.
CREATE INDEX chunks_embedding_pending ON chunks (project, id) WHERE embedding IS NULL;
