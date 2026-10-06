DROP TABLE IF EXISTS usage, crawl_jobs, chunks, links, documents, pages, sites;
DROP FUNCTION IF EXISTS jc_fts_config(text);
DROP TEXT SEARCH CONFIGURATION IF EXISTS jc_simple_unaccent;
-- The extensions stay: on the cluster CloudNativePG owns `vector`, and either may serve another
-- schema of the database.
