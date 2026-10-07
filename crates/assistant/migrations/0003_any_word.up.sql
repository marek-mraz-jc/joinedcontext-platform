-- A question's words as a full-text query that any of them matches (T-3053). `plainto_tsquery`
-- AND-s the words, and a question in a person's own words never has all of them in the passage
-- that answers it: AND-ed, the lexical ranking found the page for none of the forty eval
-- questions. Lexemes shorter than three characters are left out: `jc_simple_unaccent` has no
-- stop words, and Slovak and Czech `a`, `v`, `na`, `za`, `do` matched nearly every passage and
-- pushed the answer down the fused ranking. The lexemes the configuration produced are read
-- again by `simple`, which only lowercases what is lowercase already, so the query is built by
-- the parser and never by quoting text.
CREATE FUNCTION jc_any_word(config regconfig, question text) RETURNS tsquery
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    RETURN (
        SELECT replace(plainto_tsquery('simple', coalesce(string_agg(lexeme, ' '), ''))::text, ' & ', ' | ')::tsquery
        FROM unnest(tsvector_to_array(to_tsvector(config, question))) AS lexeme
        WHERE char_length(lexeme) >= 3
    );
