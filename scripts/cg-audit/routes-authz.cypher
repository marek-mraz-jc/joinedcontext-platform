// cg-audit 1 (T-4303): every HTTP route reaches its authorization before a store, broker or upstream call.
// Run from the repo root: cg build && cg q - < scripts/cg-audit/routes-authz.cypher
// Each statement must return only the rows listed under "expected" (false positives with the reason);
// any other row is a finding to read with SHOW before it counts.

// 1. Gateway: a handler that reaches the broker without pinning the caller (tenancy::pin_caller).
// expected: GET /api/endpoint/{}/ws (the fan-out decides per subject in fanout::hub::decide, ADR-N-049).
MATCH (h:Symbol)-[:PROVIDES]->(c:Channel {channel:'http'}) WHERE EXISTS { MATCH (h)-[:CALLS|DISPATCHES*1..8]->(b:Symbol {path:'context_gateway::proxy::Broker::send'}) } AND NOT EXISTS { MATCH (h)-[:CALLS|DISPATCHES*1..8]->(p:Symbol {path:'context_gateway::middleware::tenancy::pin_caller'}) } RETURN DISTINCT c.key, h.path;

// 2. Gateway: a handler that reaches the broker without authenticating the caller (authenticate_on).
// expected: /catalog.jsonld and /catalog.ttl (public Endpoints alone, app::public_records);
// POST .../access/simulate (the Portal's own identity, view_access::the_portal).
MATCH (h:Symbol)-[:PROVIDES]->(c:Channel {channel:'http'}) WHERE EXISTS { MATCH (h)-[:CALLS|DISPATCHES*1..8]->(b:Symbol {path:'context_gateway::proxy::Broker::send'}) } AND NOT EXISTS { MATCH (h)-[:CALLS|DISPATCHES*1..8]->(p:Symbol {name:'authenticate_on'}) } RETURN DISTINCT c.key, h.path;

// 3. Assistant: every administration handler checks the caller is the Portal, and has a test.
// expected: every row gated = true and tested = true.
MATCH (h:Symbol) WHERE h.path STARTS WITH 'assistant::chat::admin::' AND h.kind = 'Function' AND NOT h.is_test AND EXISTS { MATCH (r:Symbol {path:'assistant::chat::admin::router'})-[:CALLS]->(h) } RETURN h.path, EXISTS { MATCH (h)-[:CALLS*1..3]->(g:Symbol {path:'assistant::chat::admin::the_portal'}) } AS gated, EXISTS { MATCH (t:Symbol {is_test:true})-[:CALLS*1..3]->(h) } AS tested ORDER BY h.path;

// 4. Agent proxy: every route handler authenticates the run, or is the Portal's alone.
// expected: routes::identity::handler and ::data_credential are false (portal_only), every row tested.
MATCH (r:Symbol {path:'agent_proxy::router'})-[:CALLS]->(h:Symbol) WHERE h.path STARTS WITH 'agent_proxy::routes::' RETURN h.path, EXISTS { MATCH (h)-[:CALLS|DISPATCHES*1..4]->(g:Symbol) WHERE g.path IN ['agent_proxy::auth::authenticate','agent_proxy::auth::authenticate_data','agent_proxy::data_credential::DataCredentials::verify'] } AS authenticated, EXISTS { MATCH (h)-[:CALLS*1..3]->(p:Symbol {name:'portal_only'}) } AS portal_only, EXISTS { MATCH (t:Symbol {is_test:true})-[:CALLS*1..3]->(h) } AS tested ORDER BY h.path;

// 5. Token sidecar: the client id it mints for comes from the request, so who reaches it matters (T-4312).
// expected after T-4312: the guard refuses the token Services' names (refused_host has a row for them).
MATCH (c:Symbol)-[:CALLS]->(f:Symbol {path:'jc_core::kinds::data_source::refused_target'}) WHERE NOT c.is_test RETURN c.path;
GREP pipeline-runner-tokens|pipeline-tokens|wasm-host-tokens IN crates/jc-core/src/**/*.rs;
