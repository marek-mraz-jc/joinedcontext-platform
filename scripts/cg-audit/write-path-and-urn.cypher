// cg-audit 2 (T-4304): every write to context data goes through an Endpoint and its Policy, and
// nothing derives a space, an organization or a right from URN segments (ADR-N-041).
// Run from the repo root after `cg build`:  cg q - < scripts/cg-audit/write-path-and-urn.cypher
// Read each row with SHOW before it counts. Known false positives are listed at the end.

// 1. Every HTTP route that reaches the broker, with the guards it passes on the way.
//    A row with no guard is a finding unless it is in the false-positive list below.
MATCH (h:Symbol)-[:PROVIDES]->(c:Channel) MATCH (h)-[:CALLS|DISPATCHES*1..7]->(s:Symbol {path:'context_gateway::proxy::Broker::send'}) WITH DISTINCT h, c OPTIONAL MATCH (h)-[:CALLS|DISPATCHES*1..4]->(g:Symbol) WHERE g.path IN ['context_gateway::app::admit','context_gateway::app::admit_space','context_gateway::app::view_request','context_gateway::mcp::tools::granted','context_gateway::pdp::Pdp::decide','context_gateway::app::authenticate','context_gateway::handlers::view_access::the_portal'] RETURN c.key, h.path, collect(DISTINCT g.name) AS guards ORDER BY c.key;

// 2. Every direct caller of the broker client: each one is a write or a read through a view.
MATCH (c:Symbol)-[:CALLS]->(s:Symbol {path:'context_gateway::proxy::Broker::send'}) WHERE NOT c.is_test RETURN c.path, c.file, c.line ORDER BY c.path;

// 3. Outbound HTTP writes anywhere in the workspace: none may reach a broker except through the gateway.
MATCH (c:Symbol)-[:CALLS]->(m:Symbol) WHERE m.external AND m.name IN ['post','put','patch','delete'] AND m.path STARTS WITH 'reqwest' AND NOT c.is_test RETURN c.package, c.path, c.file, c.line, m.name ORDER BY c.package, c.path;

// 4. Who reads the segments of a parsed URN (ADR-N-041): a space or org segment feeding a decision is a finding.
MATCH (c:Symbol)-[:REFERENCES|CALLS]->(f:Symbol) WHERE f.path IN ['jc_core::urn::Urn::space','jc_core::urn::Urn::org_domain','jc_core::urn::Urn::prefixed'] AND NOT c.is_test AND NOT c.path STARTS WITH 'jc_core::urn' RETURN f.path, collect(DISTINCT c.path) AS users;

// 5. Hand-rolled colon splits outside tests; read each that splits an entity id.
GREP (split\(':'\)|splitn\([0-9], ?':'\)|rsplit(n)?\(([0-9], ?)?':'\)|split_once\(':'\)|rsplit_once\(':'\)) IN crates/**/*.rs;

// False positives (baef746, 2026-10-16), so the next run skips them:
// - view_access::access_simulate: the_portal admits only the Portal's account; it asks the view for an explain, never a write.
// - app::catalog_feed_jsonld / catalog_feed_turtle: public Endpoints only, read as the anonymous subject.
// - handlers::ogc::sensorthings, handlers::ogc::ogc_features: reads only, through the broker's view after admit.
// - pdp::reaper::Reaper::tick -> Broker::route: swaps the routing table, sends nothing.
// - mqtt::decide: device CONNECT ACL on the MQTT broker; the pipeline writes through the gateway.
// - jcctl::gateway::Broker::upsert: POSTs to the gateway, which applies the Policy.
// - jcctl::loader::of_organization / Repository::check_prefixed: the CC-78 preview naming rule, no right.
// - relationships::type_of_id, units::type_of_id, model_schema::relationship_problems: read {Type} only, which ADR-N-041 allows.
// - kpi::KeyPerformanceIndicator::validate reads Urn::space: a shape check of the id the platform mints, never a placement.
// Findings: store::bound_policy reads Urn::space to bind policyRef (T-4313).
