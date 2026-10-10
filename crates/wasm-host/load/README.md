# 10 000 Apps on jc-wasm-host

The load test of the shared host of server WASM Apps (T-3345, ADR-N-044). It places 10 000 Apps on
two shards, each with its own component, schema, role and storage prefix, then sends a long-tail
mix and reads what that costs. The result and its verdict are in ADR-N-044 §8.

It runs on a GitHub-hosted runner, never on dev and never on a shared machine: other work on the
host would be in the numbers.

| File | What it is |
|---|---|
| `compose.yaml` | the rig: two shards, apps-db and RustFS at dev's images and sizes, k6 |
| `mix.js` | the traffic: 1 % of the Apps hot, the rest rare, then a cold and a warm probe |
| `run.sh` | provision, measure idle, run the mix, sample every 15 s, report, tear down |
| `../examples/fleet.rs` | the provisioner: N component variants, schemas, roles, prefixes, placements |

## Run it

Actions → `wasm-host-10k` → Run workflow, with `apps`, `rate` (requests a second), `duration`
and `cached` (compiled components a shard keeps, dev's 700 by default). The run's artifact holds
`results.txt`, `samples.csv`, `k6-summary.json`, `fleet.log` and the shards' logs.

By hand, on a machine of its own:

```sh
cargo build --release -p wasm-host --bin jc-wasm-host --example fleet
(cd examples/apps/rust-wasm-server/server && cargo build --release --target wasm32-wasip2)
# copy jc-wasm-host, examples/fleet, rust_wasm_server.wasm and migrations/0001_notes.sql into one directory
RIG_DIR=<that directory> crates/wasm-host/load/run.sh
```

`APPS=300 RATE=10 DURATION=1m PROBE=50 WARM=200` is a two-minute smoke. The secrets are made for
one run and never printed; the volumes are removed when the script ends, also on failure.

## What it measures

- RSS of each shard: idle with its Apps placed, beside a shard with nothing placed, and during the
  mix; whether a shard was OOM-killed at dev's 1 GiB.
- The compiled-component cache: `jc_wasm_component_cache_hits_total` and `_misses_total`.
- Latency per phase: the mix (hot and rare), cold (Apps never asked before, compiled first) and
  warm; every answer that is not a success, counted by status.
- apps-db: the database and catalog size before and after provisioning, its memory, the shards'
  connections.
- RustFS: objects and bytes in the bucket.
