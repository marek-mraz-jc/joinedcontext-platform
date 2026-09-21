# agent-proxy

`jc-agent-proxy`: the one door out of an agent workspace. An assistant or builder run works in a workspace that holds no secret. Every call it makes to data, the forge, the model or a package registry goes through this proxy, which authenticates the run and injects the credential that run may use.

## 1. Entry points

- `src/main.rs`: the binary (`JC_PROXY_BIND`, `JC_OIDC_*`, `JC_FORGE_*`, `JC_MODEL_*` from the environment, see `config.rs`).
- `auth.rs`: the run ticket (`X-JC-Run` + `X-JC-Ticket`, or `Bearer jcr_…`).
- `runs.rs`: what a run may reach.
- `inject.rs`: per-endpoint tokens.
- `routes/`: `data`, `forge`, `fetch`, `packages`, `llm`, `mcp`, `inbox`, `events`, `diagnostics`.
- `limits.rs`: byte and time budgets.
- `audit.rs`: one line per call, refusals included.

## 2. Tests

    cargo test -p agent-proxy
    cargo test -p agent-proxy --test <file>

## 3. Trust boundary

The workspace is untrusted code. It chooses no host, token, project or person: the run comes from its ticket, the upstream from the run, and a fetched URL passes the allow-list on its parsed host. No credential ever reaches an answer, an error or the audit line.

## 4. Contract

`docs/Architecture/19-agent-runner.md`, `07-agents-and-mcp.md` and `docs/API/04-agent-runs.md`.
