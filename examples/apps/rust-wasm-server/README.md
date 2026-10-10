# Server WASM App: notes with files

Start every server WASM App here (T-3341, ADR-N-044). An App's server is a WebAssembly component
on the shared host `jc-wasm-host`. It costs nothing while idle: each request gets a fresh instance,
and the shard keeps only the compiled component. The App reads and writes its own Postgres schema
and its own files in RustFS through the host, as itself and no other App. It never holds a
connection string, a password or a storage key.

| Path | What it is |
|---|---|
| `server/` | the component: `wasm32-wasip2`, built on [`jc-app-sdk`](../../../crates/app-sdk) |
| `migrations/0001_notes.sql` | the App's tables; the reconciler runs migrations at publish, the App never runs DDL |

## What the server answers

The App sees the path below `/apps/{name}`:

| Route | What it does |
|---|---|
| `GET /api/notes` | the notes, newest first |
| `POST /api/notes` `{"body"}` | a new note |
| `PUT /api/notes/{id}` `{"body"}` | the note's text changed |
| `DELETE /api/notes/{id}` | the note and its file gone |
| `POST /api/notes/{id}/file` `{"name", "contentType"}` | a URL the browser uploads the file to, valid two minutes |
| `GET /api/notes/{id}/file` | a URL the browser downloads it from |

Files never pass through the App: it hands out a presigned URL for one key and one method, and
the browser talks to the store directly.

## Build and test

```sh
rustup target add wasm32-wasip2
cd server
cargo test                                         # the App's own rules, natively
cargo build --release --target wasm32-wasip2       # target/wasm32-wasip2/release/rust_wasm_server.wasm
```

`crates/wasm-host/tests/example_tests.rs` runs this component on the host against a real Postgres
and RustFS. It covers the migration as the reconciler runs it, every route, a file up and down
through presigned URLs, and a second App on the same component that sees none of the first App's
notes.

## What the SDK gives a handler

- `jc_app_sdk::http`: `Request` (method, path, query pairs, headers, body; never the caller's
  token or cookies), `Response::json`, `Response::problem`, and `Router` with `{name}` segments.
- `jc_app_sdk::sql::query` and `execute`, with bound parameters. `objects` and `rows::<T>` read
  rows as JSON objects or as your own types. A `numeric` column is cast in the query
  (`::float8`, `::text`), and a `null` parameter binds as text (`$1::int` where the column is not).
- `jc_app_sdk::gateway::get("/ngsi-ld/v1/entities?type=…")`: the App's own Endpoint, read with
  the caller's token. The host sends `http://gateway/ngsi-ld/v1/…` there and refuses every other
  origin and path, so the App names neither the gateway nor another Endpoint (AP-147).
- `jc_app_sdk::blob`: `get`, `put`, `list`, `delete` and `presign`, with keys relative to the
  App's own prefix. A key with `..` or a leading `/` is refused.

## What the host refuses

The host refuses more than one statement per call, anything but `select`, `insert`, `update`,
`delete` and `values`, `select … into`, role and setting changes, advisory locks, `notify`, large
objects, server files, and the built-ins that run a query given as text. Postgres refuses the rest:
the App's role has rights on its own schema alone. Limits per request: 64 MiB of memory, 5 s, a
2 s statement timeout, 1 000 rows. Writes stop at the App's quotas with an error that says which.
