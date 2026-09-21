# functions

`jc-functions`: the runtime of application functions, the server half of a generated app (`/apps/{name}/api/functions/{fn}`, SDK-21…SDK-23). One route, `POST /invoke`, for the Portal only. Each call brings its files, its request and the caller's token, and runs in a fresh QuickJS runtime that is thrown away afterwards.

## 1. Entry points

- `src/main.rs`: the binary. `JC_FUNCTIONS_BIND`, `JC_OIDC_ISSUER`, `JC_OIDC_JWKS_URL`, `JC_FUNCTIONS_CALLER`, `JC_FUNCTIONS_AUDIENCE` and `JC_GATEWAY_URL` come from the environment; a missing required one stops startup.
- `lib.rs`: `invoke`, the route: the token check, the size and concurrency bounds.
- `sandbox.rs`: the runtime, capped at 64 MiB, 5 s and a 1 MiB response.
- `endpoint.rs`: the one host call, to the function's own endpoint.

## 2. Tests

    cargo test -p functions

## 3. Trust boundary

The function's code is untrusted. It has no `fetch`, timer, file system or environment, can import only the files it was sent, and reaches exactly one endpoint with the caller's own token. The runtime holds no credential of its own.

## 4. Contract

`docs/Architecture/20-app-sdk.md` §3 and the SDK-21…SDK-23 requirements.
