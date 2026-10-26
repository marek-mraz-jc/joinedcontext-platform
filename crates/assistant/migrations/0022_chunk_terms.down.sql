DROP TRIGGER chunks_terms ON chunks;
DROP FUNCTION jc_chunk_terms_write();
DROP INDEX chunks_terms_pending;
ALTER TABLE chunks DROP COLUMN terms_pending;
DROP TABLE chunk_terms;
DROP FUNCTION jc_query_terms(tsquery);
DROP FUNCTION jc_chunk_terms(tsvector);
