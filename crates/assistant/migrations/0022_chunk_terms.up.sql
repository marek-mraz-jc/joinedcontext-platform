-- T-5799: the lexical arm of the hybrid search served by an index under the forced row-level
-- security. `@@` (ts_match_vq) is not leakproof, so the planner keeps it behind the policy and
-- never asks chunks_fts: the arm read every chunk of the project (2.6 s for banskabystrica's
-- 101k). A btree on (project, term) answers `term = ANY(...)` with text `=`, which is leakproof,
-- so the policy and the lookup are one index condition. No function is marked LEAKPROOF and no
-- superuser is needed.
--
-- `hits` is how often the term stands in the chunk. The question's query OR-s single lexemes of
-- weight D, so its `ts_rank_cd` is 0.1 for each position of a query lexeme in the chunk: the sum
-- of `hits` ranks the chunks as `ts_rank_cd` did, read from the index alone.
CREATE TABLE chunk_terms (
    project  text NOT NULL,
    term     text NOT NULL,
    chunk_id bigint NOT NULL REFERENCES chunks ON DELETE CASCADE,
    hits     integer NOT NULL CHECK (hits > 0),
    PRIMARY KEY (chunk_id, term)
);
CREATE INDEX chunk_terms_lookup ON chunk_terms (project, term) INCLUDE (chunk_id, hits);
ALTER TABLE chunk_terms ENABLE ROW LEVEL SECURITY;
ALTER TABLE chunk_terms FORCE ROW LEVEL SECURITY;
CREATE POLICY chunk_terms_by_project ON chunk_terms
    USING (project = current_setting('jc.project', true))
    WITH CHECK (project = current_setting('jc.project', true));

-- The terms of a chunk's `fts`. A question never asks a lexeme shorter than three characters
-- (`jc_any_word`), so none is kept.
CREATE FUNCTION jc_chunk_terms(fts tsvector) RETURNS TABLE (term text, hits integer)
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    AS $$
        SELECT lexeme, cardinality(positions) FROM unnest(fts)
        WHERE char_length(lexeme) >= 3 AND cardinality(positions) > 0
    $$;

-- The lexemes of a question's full-text query, for the lookup: its text form quotes each lexeme
-- as a tsvector does, and the `|` between them goes with every lexeme shorter than three
-- characters.
CREATE FUNCTION jc_query_terms(q tsquery) RETURNS text[]
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    RETURN ARRAY(SELECT lexeme FROM unnest(tsvector_to_array(q::text::tsvector)) AS lexeme
                 WHERE char_length(lexeme) >= 3);

-- A stored or rewritten chunk's terms, in the statement that writes it; a chunk the worker had
-- not given terms yet has them now.
CREATE FUNCTION jc_chunk_terms_write() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        DELETE FROM chunk_terms WHERE chunk_id = OLD.id;
        IF NEW.terms_pending THEN
            UPDATE chunks SET terms_pending = false WHERE id = NEW.id;
        END IF;
    END IF;
    INSERT INTO chunk_terms (project, term, chunk_id, hits)
    SELECT NEW.project, t.term, NEW.id, t.hits FROM jc_chunk_terms(NEW.fts) AS t;
    RETURN NULL;
END $$;

-- The chunks stored before this migration get their terms from the worker, a batch at a time
-- (`fill_terms`): filling 100k chunks here would hold the boot past the liveness probe. Until
-- then the lexical arm reads them as before, through this partial index.
ALTER TABLE chunks ADD COLUMN terms_pending boolean NOT NULL DEFAULT true;
ALTER TABLE chunks ALTER COLUMN terms_pending SET DEFAULT false;
CREATE INDEX chunks_terms_pending ON chunks (project, id) WHERE terms_pending;

CREATE TRIGGER chunks_terms AFTER INSERT OR UPDATE OF text, lang ON chunks
    FOR EACH ROW EXECUTE FUNCTION jc_chunk_terms_write();
