-- The knowledge assistant's store (ADR-N-040 §3.3, Architecture/22 §3, T-3050).
--
-- `vector` is not a trusted extension, so on the cluster CloudNativePG creates it for the
-- database (components/postgres, `extensions:`) and this line is a no-op; `unaccent` is trusted
-- and the database owner creates it here.
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS unaccent;

-- Slovak and Czech have no Snowball stemmer in PostgreSQL 16: their words are matched whole and
-- without diacritics, and the vectors carry what the missing stemmer loses.
CREATE TEXT SEARCH CONFIGURATION jc_simple_unaccent (COPY = simple);
ALTER TEXT SEARCH CONFIGURATION jc_simple_unaccent
    ALTER MAPPING FOR hword, hword_part, word WITH unaccent, simple;

-- The configuration a chunk's text is indexed with, from its ISO 639-1 language.
CREATE FUNCTION jc_fts_config(lang text) RETURNS regconfig
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    RETURN CASE lang
        WHEN 'en' THEN 'english'::regconfig
        WHEN 'fi' THEN 'finnish'::regconfig
        WHEN 'de' THEN 'german'::regconfig
        ELSE 'jc_simple_unaccent'::regconfig
    END;

-- One `KnowledgeSource` of one project.
CREATE TABLE sites (
    id           bigserial PRIMARY KEY,
    organization text NOT NULL,
    project      text NOT NULL,
    source       text NOT NULL,
    visibility   text NOT NULL CHECK (visibility IN ('public', 'internal')),
    last_crawl   timestamptz,
    UNIQUE (project, source)
);

CREATE TABLE pages (
    id            bigserial PRIMARY KEY,
    site_id       bigint NOT NULL REFERENCES sites ON DELETE CASCADE,
    project       text NOT NULL,
    url           text NOT NULL,
    parent_id     bigint REFERENCES pages ON DELETE SET NULL,
    depth         smallint NOT NULL CHECK (depth >= 0),
    status        text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'fetched', 'failed', 'skipped')),
    content_hash  text,
    language      text,
    included      boolean NOT NULL DEFAULT true,
    etag          text,
    last_modified text,
    fetched_at    timestamptz,
    UNIQUE (site_id, url)
);

-- PDFs and other files a page links.
CREATE TABLE documents (
    id           bigserial PRIMARY KEY,
    site_id      bigint NOT NULL REFERENCES sites ON DELETE CASCADE,
    project      text NOT NULL,
    page_id      bigint REFERENCES pages ON DELETE SET NULL,
    url          text NOT NULL,
    mime         text,
    off_domain   boolean NOT NULL DEFAULT false,
    bytes        bigint CHECK (bytes >= 0),
    pages        integer CHECK (pages >= 0),
    content_hash text,
    status       text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'fetched', 'failed', 'skipped')),
    included     boolean NOT NULL DEFAULT true,
    fetched_at   timestamptz,
    UNIQUE (site_id, url)
);

-- What a page links to, for the administration's page tree.
CREATE TABLE links (
    from_page bigint NOT NULL REFERENCES pages ON DELETE CASCADE,
    project   text NOT NULL,
    to_url    text NOT NULL,
    kind      text NOT NULL CHECK (kind IN ('page', 'document', 'external')),
    PRIMARY KEY (from_page, to_url)
);

-- The searchable text: one passage of a page or of a document, with its embedding.
CREATE TABLE chunks (
    id          bigserial PRIMARY KEY,
    site_id     bigint NOT NULL REFERENCES sites ON DELETE CASCADE,
    project     text NOT NULL,
    page_id     bigint REFERENCES pages ON DELETE CASCADE,
    document_id bigint REFERENCES documents ON DELETE CASCADE,
    ordinal     integer NOT NULL CHECK (ordinal >= 0),
    url         text NOT NULL,
    text        text NOT NULL,
    lang        text,
    visibility  text NOT NULL CHECK (visibility IN ('public', 'internal')),
    embedding   vector(384),
    fts         tsvector GENERATED ALWAYS AS (to_tsvector(jc_fts_config(lang), text)) STORED,
    CHECK ((page_id IS NULL) <> (document_id IS NULL))
);
CREATE INDEX chunks_fts ON chunks USING gin (fts);
CREATE INDEX chunks_embedding ON chunks USING hnsw (embedding vector_cosine_ops);
CREATE INDEX chunks_scope ON chunks (project, visibility, site_id);

-- The crawl queue. A worker claims with FOR UPDATE SKIP LOCKED; a job names a project and a
-- source, never their content, so the queue is the one table every project's worker reads.
CREATE TABLE crawl_jobs (
    id         bigserial PRIMARY KEY,
    project    text NOT NULL,
    source     text NOT NULL,
    state      text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'running', 'done', 'failed')),
    run_after  timestamptz NOT NULL DEFAULT now(),
    attempts   integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    claimed_by text,
    claimed_at timestamptz,
    error      text
);
CREATE INDEX crawl_jobs_ready ON crawl_jobs (run_after) WHERE state = 'queued';

-- What each deployment spent, per day.
CREATE TABLE usage (
    project    text NOT NULL,
    deployment text NOT NULL,
    day        date NOT NULL,
    requests   bigint NOT NULL DEFAULT 0 CHECK (requests >= 0),
    tokens_in  bigint NOT NULL DEFAULT 0 CHECK (tokens_in >= 0),
    tokens_out bigint NOT NULL DEFAULT 0 CHECK (tokens_out >= 0),
    PRIMARY KEY (project, deployment, day)
);

-- One project never reads another's knowledge: every tenant table holds only the rows of the
-- project the session names in `jc.project`, the table owner included (FORCE), and a session
-- that names none reads and writes nothing.
DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['sites', 'pages', 'documents', 'links', 'chunks', 'usage'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY %I ON %I USING (project = current_setting(''jc.project'', true)) '
            'WITH CHECK (project = current_setting(''jc.project'', true))',
            t || '_by_project', t);
    END LOOP;
END $$;
