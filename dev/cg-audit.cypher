// security-log-secret-field: a tracing or log macro with a field named like a credential (token, secret, authorization, password, api_key, cookie); jc_core::Secret prints nothing, a plain String prints the value
GREP (tracing::)?(trace|debug|info|warn|error)!\([^;]*\b(token|secret|authorization|password|api_key|cookie)\s*[=%?] IN **/*.rs;
// security-fetch-from-route: a function an HTTP route reaches within four calls that sends a reqwest request; each one is where a URL from the request could become an outbound fetch (SSRF)
MATCH (h:Symbol)-[:PROVIDES]->(:Channel {channel:'http'}) MATCH (h)-[:CALLS|DISPATCHES*0..4]->(c:Symbol)-[:CALLS]->(x:Symbol) WHERE NOT c.is_test AND x.external AND x.path STARTS WITH 'reqwest::' AND x.name =~ '(get|post|put|patch|delete|request)' RETURN DISTINCT c.path;
// security-mcp-write-unguarded: an MCP tool function that reaches a store write (create, upsert, mutate, delete, batch_upsert) within four calls and no admit, gate or decide call on the way (T-4590 class)
MATCH (t:Symbol)-[:CALLS|DISPATCHES*1..4]->(w:Symbol) WHERE t.file CONTAINS '/mcp/' AND NOT t.is_test AND NOT w.is_test AND w.name =~ '(create|upsert|mutate|delete|batch_upsert)' AND NOT EXISTS { MATCH (t)-[:CALLS|DISPATCHES*1..4]->(g:Symbol) WHERE g.name =~ '(admit|gate|decide|check_write)' } RETURN DISTINCT t.path;
// modularity-server-as-library: a workspace crate depending (not dev) on a crate that ships a binary (a src/main.rs main), so the server or CLI compiles into it
MATCH (a:Package)-[r:DEPENDS_ON]->(b:Package) WHERE a.workspace AND b.workspace AND NOT r.kind =~ 'dev' AND a.name <> b.name AND EXISTS { MATCH (m:Symbol) WHERE m.package = b.name AND m.path ENDS WITH '::main' AND m.file ENDS WITH 'src/main.rs' } RETURN a.name, b.name;
// modularity-file-cycle: two files of one crate that call each other (a module cycle), outside tests
MATCH (a:Symbol)-[:CALLS]->(b:Symbol) WHERE a.package = b.package AND a.file < b.file AND NOT a.is_test AND NOT b.is_test AND NOT a.external AND NOT b.external AND EXISTS { MATCH (c:Symbol)-[:CALLS]->(d:Symbol) WHERE c.file = b.file AND d.file = a.file AND NOT c.is_test } RETURN DISTINCT a.file, b.file;
// memory-body-copy: a body, payload or bytes value copied whole (to_vec, clone, to_owned) in production code
GREP \b\w*(body|bytes|payload)\w*\.(to_vec|clone|to_owned)\(\) IN **/*.rs;
// memory-unbounded-global: a process-wide growable container (a static, LazyLock or OnceLock of a map, set or Vec), which only shrinks if its code evicts
GREP (static|LazyLock|OnceLock)[^\n]*(HashMap|BTreeMap|HashSet|BTreeSet|DashMap|Vec)< IN **/*.rs;
// memory-uncapped-read: a whole outbound response body read with no size cap (reqwest bytes, text or json awaited, to_bytes with usize::MAX)
GREP (\.(bytes|text|json(::<[^>]*>)?)\(\)\.await|to_bytes\([^)]*usize::MAX) IN **/*.rs;
// memory-per-request-build: a Regex or a reqwest client built in a function an HTTP route reaches within four calls, so each request builds it again
MATCH (h:Symbol)-[:PROVIDES]->(:Channel {channel:'http'}) MATCH (h)-[:CALLS|DISPATCHES*0..4]->(c:Symbol)-[:CALLS]->(x:Symbol) WHERE NOT c.is_test AND x.external AND ((x.path ENDS WITH 'Regex::new') OR (x.path STARTS WITH 'reqwest::' AND x.path CONTAINS 'Client' AND x.name =~ '(new|builder)')) RETURN DISTINCT c.path, x.path;
