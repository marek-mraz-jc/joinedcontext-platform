# Server WASM App: notes with files

Start every server WASM App here (T-3341, ADR-N-044). An App's server is a WebAssembly component
on the shared host `jc-wasm-host`. It costs nothing while idle: each request gets a fresh instance,
and the shard keeps only the compiled component. The App reads and writes its own Postgres schema
and its own files in RustFS through the host, as itself and no other App. It never holds a
connection string, a password or a storage key.

| Path | What it is |
|---|---|
| `server/` | the component: `wasm32-wasip2`, built on [`jc-app-sdk`](../../../crates/app-sdk) |
| `migrations/` | the App's tables, `0002_roles.sql` adds owners, the notice and `app_audit`; the reconciler runs migrations at publish, the App never runs DDL |

## What the server answers

The App sees the path below `/apps/{name}`:

| Route | Who | What it does |
|---|---|---|
| `GET /api/notes` | signed in | the caller's notes (a steward: all), newest first |
| `POST /api/notes` `{"body"}` | signed in | a new note of the caller's |
| `PUT /api/notes/{id}` `{"body"}` | owner, steward | the note's text changed |
| `DELETE /api/notes/{id}` | owner, steward | the note and its file gone |
| `POST /api/notes/{id}/file` `{"name", "contentType", "size"}` | owner, steward | a URL the browser uploads the file of exactly `size` bytes to, valid two minutes |
| `GET /api/notes/{id}/file` | owner, steward | a URL the browser downloads it from |
| `GET /api/notice` | anyone | the shared notice |
| `PUT /api/notice` `{"body"}` | steward | the notice changed |
| `GET /api/history` | steward | the newest changes: who, what, when |
| `PATCH /api/entities/{id}` `{attrs}` | the Endpoint's Policy | Attributes of one Entity changed, as the caller |

Roles come from `identity::caller()` alone (API/07): a role in the body or a header is ignored. A
note another person wrote answers `404`, as if it did not exist. Every change writes a row into
`app_audit` in the same statement, so a failed change leaves no history.

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
notes. A second test runs an anonymous caller, a viewer and a steward through every route above.

## What the SDK gives a handler

- `jc_app_sdk::http`: `Request` (method, path, query pairs, headers, body; never the caller's
  token or cookies), `Response::json`, `Response::problem`, and `Router` with `{name}` segments.
- `jc_app_sdk::sql::query` and `execute`, with bound parameters. `objects` and `rows::<T>` read
  rows as JSON objects or as your own types. A `numeric` column is cast in the query
  (`::float8`, `::text`), and a `null` parameter binds as text (`$1::int` where the column is not).
- `jc_app_sdk::gateway::get("/ngsi-ld/v1/entities?type=…")`: the App's own Endpoint, read with
  the caller's token. The host sends `http://gateway/ngsi-ld/v1/…` there and refuses every other
  origin and path, so the App names neither the gateway nor another Endpoint (AP-147).
- `jc_app_sdk::access`: `Caller::require` (401), `require_role` (403 naming the role needed),
  `Owned` for rows a person writes (owner or editor role changes them, history in the same
  statement), `record` and `history` for the App's `app_audit` table.
- `jc_app_sdk::blob`: `get`, `put`, `list`, `delete` and `presign`, with keys relative to the
  App's own prefix. A key with `..` or a leading `/` is refused.
- `jc_app_sdk::gateway`: `get` and `get_json` of a gateway path such as
  `/api/endpoint/{slug}/ngsi-ld/v1/entities?type=…`, as the caller: the host adds the caller's
  token and sends `http://gateway` to the gateway it runs with. A `401` or `403` passes through to
  the caller as `Response::from(err)`; anything else the gateway answers is a `502`.

## What the host refuses

The host refuses more than one statement per call, anything but `select`, `insert`, `update`,
`delete` and `values`, `select … into`, role and setting changes, advisory locks, `notify`, large
objects, server files, and the built-ins that run a query given as text. Postgres refuses the rest:
the App's role has rights on its own schema alone. Limits per request: 64 MiB of memory, 5 s, a
2 s statement timeout, 1 000 rows. Writes stop at the App's quotas with an error that says which.
