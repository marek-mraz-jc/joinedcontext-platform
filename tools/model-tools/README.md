# Model Tools

The one non-Rust runtime component (Architecture/11 §6.5). LinkML and schema-automator are
Python, so the generators live here; everything with state or authority stays in Rust.

Model Tools is a pure function: it reads a LinkML document and writes artifacts. It holds no
credentials, reads no platform state and opens no socket except `import_sdm`, which reaches
the Smart Data Models organisation and nothing else (DM-10, DM-18).

| Script | Renders | Requirements |
|---|---|---|
| `src/gen_json_schema.py` | JSON Schema draft-07, with the NGSI-LD kind and the UN/CEFACT unit per slot | DM-02, DM-03, DM-05, DM-06 |
| `src/gen_context.py` | JSON-LD `@context` with bound IRIs, `@type: @id` for Relationships, `@container: @language` for LanguageProperties | DM-04, DM-05, DM-16 |
| `src/gen_rdf_artifacts.py` | closed SHACL shapes and the OWL ontology, both Turtle | DM-28, DM-43, DM-46 |
| `src/gen_typescript.py` | `jc-types.ts` for a generated application: one type per entity class, each attribute as the App SDK's row cell | SDK-03, SDK-10 |
| `src/import_sdm.py` | a LinkML model from a Smart Data Models catalogue identifier, with provenance | DM-07…DM-11 |
| `src/infer_schema.py` | a draft LinkML model and the editor operations that build it, from a sample file parsed in memory | DM-54, DM-55 |

`models/ngsi-ld-core.linkml.yaml` is the shared import: a model writes `imports:
[ngsi-ld-core]` and `is_a: Entity`, and inherits `id`, `type`, `location` and `observedAt`
(DM-09). Imports are merged when a model is loaded, so every artifact is self-contained and
resolvable without a network.

```bash
python3 src/gen_json_schema.py model.linkml.yaml -o json-schema/model.v1.json
python3 src/gen_context.py model.linkml.yaml -o context/model.v1.jsonld
python3 src/gen_rdf_artifacts.py model.linkml.yaml --artifact shacl -o shapes/model.v1.ttl
python3 src/gen_typescript.py model.linkml.yaml -o src/jc-types.ts
python3 src/import_sdm.py dataModel.Environment/AirQualityObserved -o model.linkml.yaml
pytest
```

## The HTTP face

`src/service.py` is the same functions over HTTP, which is how the Portal reaches them
(API/01 §11, DM-17). It has no ingress, no session and no credentials: the Portal is the only
caller and it proxies the browser.

| Route | Body | Answers |
|---|---|---|
| `GET /healthz` | — | `{"status", "generatorVersion"}`, what a readiness probe reads |
| `GET /catalog?refresh=true` | — | the Smart Data Models index, cached daily (DM-12) |
| `POST /generate` | `{"source"}` | `jsonSchema`, `context`, `shacl`, `owl`, `generatorVersion`, `errors` |
| `POST /import-sdm` | `{"model"}` | the same, plus the `linkml` the import produced and its `example` |
| `POST /infer-schema` | `{"name", "content" (base64), "format"?}` | a draft model from a CSV, XLSX, JSON or PDF sample: `linkml`, `operations`, `detectedTypes`, `matches`, `untyped`, `rows` (DM-54) |

A source that does not compile is `200` with `errors` and no artifacts: a half-written model is
the normal state of an editor. A body past 512 KiB is `413` (`/infer-schema` takes a 10 MiB
sample, DM-55), and an identifier that is not `dataModel.<Subject>/<Model>` is `400`, refused
before a socket exists (DM-10, DM-18).

```bash
MODEL_TOOLS_PORT=8080 python3 src/service.py
curl -s localhost:8080/healthz
jq -Rs '{source: .}' tests/fixtures/senzor.linkml.yaml | curl -s -d @- localhost:8080/generate
```

## The image

```bash
docker build -t model-tools tools/model-tools     # from the repository root
docker run --rm -p 8080:8080 model-tools
```

`.github/workflows/image.yml` publishes it as
`ghcr.io/marek-mraz-jc/joinedcontext-platform/model-tools`, signs it and scans it, and the lane
runs the built image against a real model before signing: the failure this catches is
packaging, not code, because an image that cannot resolve the shipped `ngsi-ld-core` import
answers with errors and no artifacts and every unit test still passes.

The image is `linux/amd64`. `py-horned-owl`, which LinkML's OWL generator needs, publishes no
`aarch64` wheel, so an arm64 build compiles it from source and needs a Rust toolchain; build
with `--platform linux/amd64` on an Apple Silicon machine.
