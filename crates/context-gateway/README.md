# context-gateway

The only way into context data (R1, ADR-N-003): an axum service with an in-process PDP in front of every context broker. It resolves an endpoint slug to its space and policies, establishes the caller from the verified token, narrows the query, and projects the answer. It serves the NGSI-LD, OGC Features, SensorThings, file and MCP representations and the `schema/` surface.

## 1. Entry points

- `src/main.rs`: the binary (`JC_GATEWAY_*` and `JC_OIDC_*` from the environment, see `config.rs`).
- `app.rs`: the router and the NGSI-LD pipeline.
- `pdp/`: the verdicts.
- `auth/`: tokens and ServiceAccounts.
- `handlers/`, `translators/`: the other representations.
- `mcp/`: the data MCP.
- `store.rs`: the endpoint table, loaded from the configuration repository.

## 2. Tests

    cargo test -p context-gateway
    cargo test -p context-gateway --test <file>

## 3. Trust boundary

Every request is untrusted: whatever the client says about its identity, tenant or scope is stripped, and both are established from the endpoint and the verified token. Nothing the grants do not cover reaches the wire, and a broker error is never passed through as it came.

## 4. Contract

`docs/Architecture/05-context-gateway.md`, `04-context-spaces-and-endpoints.md` and `docs/API/02-endpoint-representations.md`.
