// T-4308 cg-audit 6: panics and unbounded work reachable from anonymous routes.
// The handlers of the public route tables: context-gateway app.rs and handlers/*, the MCP hub and
// named servers, and jc-assistant's /api/v1/d and /d routes (routes are not linked in .cg/links,
// so they are named here). Run: cg q scripts/cg-audit/public-panics.cypher
// Q1: every non-test symbol a public handler reaches (6 hops, dynamic dispatch included) that calls
// unwrap/expect or a panicking macro; each row is then read with SHOW and classified in T-4308.
MATCH (h:Symbol)-[:CALLS|DISPATCHES*1..6]->(x:Symbol)-[r:CALLS]->(p:Symbol)
WHERE h.path IN ['assistant::chat::chat','assistant::chat::feedback','assistant::chat::preflight','assistant::chat::widget::full_page','assistant::chat::widget::launcher','assistant::chat::widget::page','context_gateway::app::catalog_feed_jsonld','context_gateway::app::catalog_feed_turtle','context_gateway::app::endpoint_record','context_gateway::app::mcp_message','context_gateway::app::ngsi_ld','context_gateway::app::ngsi_ld_entry','context_gateway::app::space_mcp_message','context_gateway::app::space_ngsi_ld','context_gateway::handlers::files::file_csv','context_gateway::handlers::files::file_geojson','context_gateway::handlers::files::file_json','context_gateway::handlers::files::file_xlsx','context_gateway::handlers::files::file_zip','context_gateway::handlers::files::space_dump','context_gateway::handlers::ogc::odata','context_gateway::handlers::ogc::ogc_features','context_gateway::handlers::ogc::sensorthings','context_gateway::handlers::schema_routes::schema_index','context_gateway::handlers::schema_routes::space_schema_index','context_gateway::handlers::space_routes::space_catalog','context_gateway::handlers::space_routes::space_record','context_gateway::handlers::view_access::access','context_gateway::handlers::view_access::access_check','context_gateway::mcp::hub::endpoints','context_gateway::mcp::hub::message','context_gateway::mcp::server::message']
  AND NOT x.is_test AND NOT x.external AND x.package IN ['context-gateway','assistant','jc-core']
  AND p.name IN ['unwrap','expect','panic','unreachable','unimplemented','todo']
RETURN DISTINCT x.path, x.file, r.line, p.name ORDER BY x.file, r.line;
// Q2: str/slice range indexing in non-test gateway and assistant code (the graph has no edge for
// indexing). A row is a defect when the index walks bytes over request text and the slice is a
// str; T-4323 (translators::sta::conjuncts) was found this way. Classified rows are in T-4308.
GREP \[[a-z_0-9.()+ -]*\.\.[a-z_0-9.()+ -]*\] IN crates/context-gateway/src/**/*.rs, crates/assistant/src/**/*.rs;
// Q3: the inputs' caps, to check that every count, page and depth a public caller sends has one.
GREP const [A-Z_]*(CAP|MAX|LIMIT)[A-Z_]*: IN crates/context-gateway/src/**/*.rs, crates/assistant/src/chat/**/*.rs;
