-- T-6183: where a passage sits in its source, read from the address it cites, so a search can
-- keep to a part of a website or a folder of uploaded files and a hit can say where it is from.
-- No column: the address holds both since T-3314 (a PDF passage cites `…#page=N`) and MF-51 (an
-- uploaded file cites `upload:{source}/{folders}/{name}`), so no stored chunk is rewritten.

-- A website page's URL path, an uploaded file's folders and name after its source; `/` for a
-- site's root. Query and fragment go.
CREATE FUNCTION jc_chunk_path(url text) RETURNS text
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    RETURN COALESCE(NULLIF(regexp_replace(
        regexp_replace(url, '^(upload:[^/]*|[A-Za-z][A-Za-z0-9+.-]*://[^/?#]*)', ''),
        '[?#].*$', ''), ''), '/');

-- The PDF page a passage starts on, from its `#page=N`; NULL for any other passage.
CREATE FUNCTION jc_chunk_page(url text) RETURNS integer
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    RETURN substring(url FROM '#page=([1-9][0-9]{0,8})$')::integer;
