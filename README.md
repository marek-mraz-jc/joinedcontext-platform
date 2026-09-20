# joinedcontext-platform

The Rust workspace of the joinedcontext platform: the shared manifest model, the Context
Gateway that stands in front of every context broker, and `jcctl`, the reconciler that turns a
Git repository of manifests into a running installation. Nothing here talks to a person — the
management application is [joinedcontext-portal](https://github.com/marek-mraz-jc/joinedcontext-portal),
and the specification these crates implement is the `docs` repository.

## 1. What is in it

| Crate | Chapter | Role |
|---|---|---|
| `crates/jc-core` | Architecture/03, 06 | the shared model: the URN scheme, the manifest kinds (JSON Schemas in `schemas/kinds/`), the `Policy` model, the errors |
| `crates/context-gateway` | Architecture/05, 04 | the enforcement point and its in-process PDP: tenant pinning, query narrowing, answer projection, the NGSI-LD, OGC, SensorThings, file and MCP representations, the `schema/` and `access` surfaces |
| `crates/jcctl` | Architecture/06 | the reconciler, as a library and a CLI: plan, apply, export, import, sync, drift, and the APISIX standalone rendering. The Portal embeds the library; the CLI is for operators and CI |
| `tools/model-tools` | Architecture/11 | the one non-Rust component: a Python image running the LinkML generators, the Smart Data Models import and the LinkML-Map compiler, statelessly |

## 2. How it fits

A request from outside reaches APISIX, then the Context Gateway, then the broker. The gateway
is the only thing that decides what a caller may read or write, and it decides it from the
`Policy` manifests `jcctl` compiled out of the organization's Git repository. Start at
[Architecture/05 — Context Gateway](https://github.com/marek-mraz/joinedcontext-docs/blob/main/Architecture/05-context-gateway.md)
for the enforcement path and
[Architecture/06 — Configuration as Code](https://github.com/marek-mraz/joinedcontext-docs/blob/main/Architecture/06-configuration-as-code.md)
for how a manifest becomes a running endpoint.

## 3. Build

```bash
cargo build --locked
```

## 4. Test

The fast checks, which are the ones the merge gate runs:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --lib --bins --locked
```

An integration test is its own binary and is not in `--lib --bins`; run the one you touched:

```bash
cargo test -p context-gateway --test projection_tests
```

The whole suite, the audit and the policy lanes run hourly in `ci-full.yml`.

## 5. Run locally

The gateway needs a broker to stand in front of and a realm to verify tokens against, both
named by environment variables:

```bash
JC_GATEWAY_BROKER_URL=http://127.0.0.1:1026 \
JC_GATEWAY_ORG_DOMAIN=example.org \
JC_GATEWAY_REPO_DIR=./my-organization \
JC_OIDC_ISSUER=https://idm.example.org/realms/joinedcontext \
cargo run -p context-gateway
```

Without `JC_OIDC_ISSUER` the gateway has no realm to verify a token against and serves public
endpoints only; every presented token is refused rather than believed.

`jcctl` needs only a checkout of an organization repository, and `jcctl` with no argument
prints every command it has:

```bash
cargo run -p jcctl -- validate --repo-dir ./my-organization
cargo run -p jcctl -- plan --repo-dir ./my-organization
```

`validate` and `plan` read and print; `apply` writes to a cluster and is the command to be
careful with.

## 6. Security

Report a vulnerability privately through this repository's GitHub security advisories, or by
the contact in [SECURITY.md](SECURITY.md). Please do not open a public issue for one.

Two things are worth knowing before you read the code: the gateway is the enforcement point,
so a change to `crates/context-gateway/src/pdp/` or `src/app.rs` is a change to who can read
what; and a secret never appears in a manifest, an image or a log — it is named by a
`secretRef` and resolved by the reconciler.

## 7. Working here

Read the owning chapter and requirement family before writing code, cite the requirement ids
in the commit message, and ship the tests with the change. `cargo clippy -D warnings` and the
tests gate every push.
