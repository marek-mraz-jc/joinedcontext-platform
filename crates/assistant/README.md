# assistant

The knowledge assistant's store (`jc-assistant`, [ADR-N-040](https://github.com/marek-mraz-jc/joinedcontext-docs/blob/main/Decisions/adr-n-040-knowledge-assistant.md)): the migrations of its PostgreSQL database and the one hybrid retrieval query. The crawl worker and the chat API build on it.

- **Migrations**: `migrations/*.up.sql` and `*.down.sql`, embedded in the library as `assistant::MIGRATOR`. They create the `unaccent` extension and expect `vector` (pgvector), which is not a trusted extension: on the cluster CloudNativePG creates it for the `assistant` database.
- **Tenancy**: every table holding a project's knowledge has row-level security forced on its owner too. A transaction reads and writes one project's rows after `assistant::project_scope(pool, project)`; a session that names no project reads and writes nothing.
- **Retrieval**: `assistant::hybrid_search` ranks a question by full-text search (English, Finnish and German stemmers; Slovak and Czech whole words without diacritics) and by cosine distance on 384-dimension embeddings, and fuses the two rankings by reciprocal rank (`1 / (60 + rank)`).

## Tests

The store tests need a PostgreSQL with pgvector whose user may create databases. The cluster's own image has no entrypoint, so start it by hand:

```sh
docker run -d --name assistant-pg -p 5433:5432 --entrypoint bash \
  ghcr.io/cloudnative-pg/postgis:16-3.6-system-trixie \
  -c 'export PATH=/usr/lib/postgresql/16/bin:$PATH; initdb -D /tmp/pg -U postgres --auth=trust >/dev/null && echo "host all all 0.0.0.0/0 trust" >> /tmp/pg/pg_hba.conf && exec postgres -D /tmp/pg -c listen_addresses=* -c unix_socket_directories=/tmp'
JC_ASSISTANT_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5433/postgres cargo test -p assistant
```

Without the variable the store tests fail and say what to set; `ci-full` starts the same image.
