# joinedcontext-platform

Rust workspace of the joinedcontext platform: shared model, Context Gateway and the reconciler (specification: `../docs`). The Portal (API + UI) lives in `../joinedcontext-portal`.

| Crate | Chapter | Role |
|---|---|---|
| `crates/jc-core` | Architecture/03, 06 | shared model: URN scheme, manifest kinds (JSON Schemas in `schema/kinds/`), `Policy` model, errors |
| `crates/context-gateway` | Architecture/05, 04 | PEP + in-process PDP, tenant injection, representation translation, MCP façade, `schema/` and `access` surfaces |
| `crates/jcctl` | Architecture/06 | reconciler library + CLI: plan/apply/export/import/sync/drift, APISIX standalone rendering; the Portal embeds the library, the CLI is for operators and CI |
| `tools/model-tools` | Architecture/11 | Python image: LinkML generators, Smart Data Models import, LinkML-Map compiler (stateless) |

Rules: read the owning chapter and requirement family before coding; cite requirement IDs in commits; `cargo clippy -D warnings` and tests gate every PR.
