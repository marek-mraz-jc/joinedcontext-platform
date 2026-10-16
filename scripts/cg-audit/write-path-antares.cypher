// cg-audit 2 (T-4304), the Antares half: every write route reaches the Policy decision.
// Run in an Antares clone (origin/dev) after `cg build`:
//   cg q - < /path/to/joinedcontext-platform/scripts/cg-audit/write-path-antares.cypher

// 1. Write routes that never reach policy::decide. Expected: only the rows listed below.
MATCH (h:Symbol)-[:PROVIDES]->(c:Channel) WHERE c.key =~ '(POST|PATCH|DELETE|PUT) .*' AND NOT EXISTS { MATCH (h)-[:CALLS|DISPATCHES*1..5]->(g:Symbol) WHERE g.path IN ['antares_api::policy::decide','antares_api::policy::decide_each'] } RETURN c.key, h.path, h.file, h.line ORDER BY c.key;

// 2. Write routes that reach a store write, with whether they reach the decision.
MATCH (h:Symbol)-[:PROVIDES]->(c:Channel) WHERE NOT c.key STARTS WITH 'GET' MATCH (h)-[:CALLS|DISPATCHES*1..6]->(w:Symbol) WHERE w.path STARTS WITH 'antares_sql::store::any::AnyStore::' AND w.name IN ['create','upsert','delete','batch_create','batch_upsert','batch_delete'] WITH DISTINCT h, c OPTIONAL MATCH (h)-[:CALLS|DISPATCHES*1..5]->(g:Symbol) WHERE g.path IN ['antares_api::policy::decide','antares_api::policy::decide_each'] RETURN c.key, h.path, count(g) AS guarded ORDER BY guarded, c.key;

// 3. Hand-rolled colon splits outside tests (ADR-N-041: no tenant or right from URN segments).
GREP (split\(':'\)|splitn\([0-9], ?':'\)|rsplit(n)?\(([0-9], ?)?':'\)|split_once\(':'\)|rsplit_once\(':'\)|strip_prefix\("urn:ngsi-ld:) IN crates/**/*.rs;

// Expected rows of 1 (dev 5d0c44e2, 2026-10-16), each read with SHOW:
// - /q/tenants/{}, /q/dead-letters/{}(/replay): admin_auth::require (T-3569); open only when
//   ANTARES_ADMIN_TOKEN_FILE is unset, which the deployment sets (components/context-broker).
// - POST /ex/v1/remote-notify: keyed by the per-copy UUID; a non-peer gets 404 (T-3569 decision).
// - PUT|PATCH /ngsi-ld/v1/entities: missing_entity_id, an error answer.
// False positives of 3: jsonld/linkml CURIE expansion, EntityId::new (shape), mcp::cursor::decode
// (tool:digest:offset), mqtt endpoint parse (host:port), views::quality::expanded (CURIE).
