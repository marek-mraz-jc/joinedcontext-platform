# jc-core

The shared model of the platform: the URN scheme of every entity id, the manifest kinds as Rust types with their JSON Schemas, the `Policy` model and the errors. The Context Gateway, `jcctl` and the Portal all read manifests through it, so a rule written here holds at every door.

## 1. Entry points

- `kinds/`: one module per kind (`Endpoint`, `DataModel`, `Mapping`, `Pipeline`, `App`, …). Each has `validate()`, the check a manifest must pass before anything acts on it.
- `registry.rs`: kind name → schema, repository path and validation; `validate_yaml` is what `jcctl validate` calls.
- `urn.rs`: `urn:ngsi-ld:{Type}:{orgDomain}:{space}:{localId}` (PF-10, PF-42).
- `names.rs`: DNS-1123 labels, namespaces, space names, local ids, entity types and relative paths.
- `envelope.rs`: `apiVersion`/`kind`/`metadata`/`spec` with `deny_unknown_fields`.

## 2. Tests

    cargo test -p jc-core

## 3. Trust boundary

Every manifest a person, an agent or an import wrote is untrusted until its kind's `validate()` has passed. Parsing alone is not validation: a caller that deserializes a spec and skips `validate()` has skipped the check.

## 4. Contract

`docs/Architecture/03-domain-model.md` and `06-configuration-as-code.md`; the generated schemas in `schemas/kinds/`.
