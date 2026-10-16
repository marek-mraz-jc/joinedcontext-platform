// cg-audit 3 (T-4305): secrets and tokens never reach logs, errors, events or responses.
// Run: cg build && cg q - < scripts/cg-audit/secrets-in-logs.cypher
// Every query returns no rows on a clean tree. A row is a finding to read with SHOW.

// 1. A struct that derives Debug and holds a credential as a plain string. Wrap the field in
//    jc_core::secret::Secret (or the crate's own redacting type) or write a redacting Debug.
//    Left out: a test double whose doc says "the test double" (jcctl InMemoryCkan), a page
//    cursor (continue_token) and the name of an image pull Secret (portal Settings.pull_secret).
//    The same file runs in joinedcontext-portal and Antares (cg q - < this file in that clone).
MATCH (s:Symbol {lang:'rust', kind:'Struct'})-[:CONTAINS]->(f:Symbol {kind:'Field'}) WHERE NOT s.is_test AND NOT s.external AND NOT s.doc CONTAINS 'the test double' AND NOT f.name IN ['continue_token','pull_secret'] AND s.attrs CONTAINS 'Debug' AND f.name =~ '(?i)(.*_)?(secret|token|password|passwd|api_key|apikey|authorization|bearer|private_key|key_pem|model_key)' AND f.signature =~ '.*: (pub )?(Option<)?(String|&.*str)>?$' RETURN s.path, f.name, f.signature, s.file, s.line;

// 2. A function that reads a secret's plaintext and logs in the same body: read it with SHOW,
//    the value must not be a field of the log line.
MATCH (c:Symbol)-[:CALLS]->(e:Symbol) WHERE e.name IN ['expose','expose_secret'] AND NOT e.external AND NOT c.is_test WITH DISTINCT c MATCH (c)-[:CALLS]->(m:Symbol) WHERE m.path IN ['tracing::macros::debug','tracing::macros::info','tracing::macros::warn','tracing::macros::error'] RETURN c.path, c.file, c.line;
