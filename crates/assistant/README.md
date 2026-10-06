# assistant

The knowledge assistant's store (`jc-assistant`, [ADR-N-040](https://github.com/marek-mraz-jc/joinedcontext-docs/blob/main/Decisions/adr-n-040-knowledge-assistant.md)): the migrations of its PostgreSQL database and the one hybrid retrieval query. The crawl worker and the chat API build on it.

- **Migrations**: `migrations/*.up.sql` and `*.down.sql`, embedded in the library as `assistant::MIGRATOR`. They create the `unaccent` extension and expect `vector` (pgvector), which is not a trusted extension: on the cluster CloudNativePG creates it for the `assistant` database.
- **Tenancy**: every table holding a project's knowledge has row-level security forced on its owner too. A transaction reads and writes one project's rows after `assistant::project_scope(pool, project)`; a session that names no project reads and writes nothing.
- **Retrieval**: `assistant::hybrid_search` ranks a question by full-text search (English, Finnish and German stemmers; Slovak and Czech whole words without diacritics) and by cosine distance on 384-dimension embeddings, and fuses the two rankings by reciprocal rank (`1 / (60 + rank)`). The question's words are OR-ed, words shorter than three characters left out (`jc_any_word`).
- **Embeddings**: `assistant::embed` runs `multilingual-e5-small` int8 (`passage: ` and `query: ` prefixes) from the files `scripts/ci/e5-small.sh` fetches, each checked against its SHA-256 before it loads, on the ONNX Runtime `scripts/ci/onnxruntime.sh` fetches (`ORT_DYLIB_PATH`). The worker embeds every chunk that has no embedding yet, once a minute; a changed page's chunks are replaced, so they are embedded again.
- **Eval**: `tests/eval_tests.rs` asks forty questions in Slovak, Czech, Finnish and English of `tests/fixtures/eval/corpus.json` and prints recall@5 and recall@10 of the lexical ranking, the vector ranking and the hybrid query (`--nocapture`). Hybrid falling below either, or below 0.85 at 5, fails.

## Tests

The store tests need a PostgreSQL with pgvector whose user may create databases. The cluster's own image has no entrypoint, so start it by hand:

```sh
docker run -d --name assistant-pg -p 5433:5432 --entrypoint bash \
  ghcr.io/cloudnative-pg/postgis:16-3.6-system-trixie \
  -c 'export PATH=/usr/lib/postgresql/16/bin:$PATH; initdb -D /tmp/pg -U postgres --auth=trust >/dev/null && echo "host all all 0.0.0.0/0 trust" >> /tmp/pg/pg_hba.conf && exec postgres -D /tmp/pg -c listen_addresses=* -c unix_socket_directories=/tmp'
sh scripts/ci/e5-small.sh /tmp/e5-small && sh scripts/ci/onnxruntime.sh /tmp/onnxruntime
JC_ASSISTANT_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5433/postgres \
JC_ASSISTANT_TEST_MODEL_DIR=/tmp/e5-small ORT_DYLIB_PATH=/tmp/onnxruntime/libonnxruntime.so \
  cargo test -p assistant
```

Without the variables the store, embedding and eval tests fail and say what to set; `ci-full` starts the same image and fetches the same model.
