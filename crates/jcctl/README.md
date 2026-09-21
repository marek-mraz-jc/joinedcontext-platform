# jcctl

The reconciler, as a library and a CLI. It loads a Git repository of manifests, validates it, and plans and applies it in waves: broker tenants, Policy entities, Bento configurations, `apisix.yaml`, app Deployments, seed entities. It also exports, imports, syncs and reports drift. The Portal embeds the library; the CLI is for operators and CI.

## 1. Entry points

- `src/main.rs`: the `jcctl` binary (`validate`, `plan`, `apply`, `export`, `import`, `publish ckan`, `artifacts rebuild`, …; API/03).
- `loader.rs`: `Repository::load`, the walk of a checkout (links out of the tree refused).
- `commands/`: one module per subcommand.
- `apisix.rs`, `bento.rs`, `roles.rs`: renderers.
- `secrets/`: SOPS+age and OpenBao resolution of `secretRef`.
- `gateway.rs`: the one write to a running platform, through the Context Gateway.

## 2. Tests

    cargo test -p jcctl
    cargo test -p jcctl --test <file>     # one file under tests/

## 3. Trust boundary

Everything in the repository is input written by people and agents: paths, names and templates are checked before they reach the filesystem, a renderer or the broker. The only credential it holds is its own ServiceAccount token, sent in the `Authorization` header and never printed.

## 4. Contract

`docs/API/03-jcctl.md` and `docs/Architecture/06-configuration-as-code.md`.
