//! The assistant's store against a real PostgreSQL with pgvector (T-3050, ADR-N-040 §3.3).
//!
//! `JC_ASSISTANT_TEST_DATABASE_URL` names a server whose user may create databases: the
//! cluster's own image (`ghcr.io/cloudnative-pg/postgis:16-3.6-system-trixie`, pinned in
//! deployment's components/postgres/images.yaml) in `ci-full` and locally:
//!
//! ```text
//! docker run -d -p 5433:5432 -e POSTGRES_PASSWORD=pw ghcr.io/cloudnative-pg/postgis:16-3.6-system-trixie
//! JC_ASSISTANT_TEST_DATABASE_URL=postgres://postgres:pw@127.0.0.1:5433/postgres cargo test -p assistant --test store_tests
//! ```
//!
//! The fast `ci` lane runs `--lib --bins` only, so these never run without their database: a
//! missing variable is a failure that says what to set, never a test that passes by skipping.

use assistant::{
    endpoint_page, hybrid_search, project_scope, titles, Search, DIMENSIONS, MIGRATOR,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};

/// A fresh database on the test server with the migrations applied, and its name.
async fn database(test: &str) -> (PgPool, PgPool, String) {
    let url = std::env::var("JC_ASSISTANT_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!("set JC_ASSISTANT_TEST_DATABASE_URL to a PostgreSQL with pgvector whose user may create databases (see this file's header)")
    });
    let admin_options: PgConnectOptions =
        url.parse().expect("JC_ASSISTANT_TEST_DATABASE_URL parses");
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(admin_options.clone())
        .await
        .expect("the test server answers");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let name = format!("assistant_{test}_{}_{nanos}", std::process::id());
    // The name is the test's own word and numbers, never input.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create the test database");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(admin_options.database(&name).disable_statement_logging())
        .await
        .expect("connect to the test database");
    MIGRATOR.run(&pool).await.expect("the migrations apply");
    // The service's own role: not a superuser, so row-level security holds for it.
    sqlx::query("DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'assistant_app') THEN CREATE ROLE assistant_app NOLOGIN; END IF; END $$")
        .execute(&pool).await.expect("the app role");
    sqlx::query(
        "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO assistant_app",
    )
    .execute(&pool)
    .await
    .expect("grant tables");
    sqlx::query("GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO assistant_app")
        .execute(&pool)
        .await
        .expect("grant sequences");
    (admin, pool, name)
}

async fn drop_database(admin: PgPool, pool: PgPool, name: &str) {
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .expect("drop the test database");
}

/// A unit vector along `axis`, or a mix of two when `toward` is given.
fn embedding(axis: usize, toward: Option<(usize, f32)>) -> Vec<f32> {
    let mut v = vec![0.0; DIMENSIONS];
    v[axis] = 1.0;
    if let Some((other, weight)) = toward {
        v[other] = weight;
    }
    v
}

/// Inserts a site of `project` and its chunks, as the app role in that project's scope.
async fn site(
    pool: &PgPool,
    project: &str,
    source: &str,
    chunks: &[(&str, &str, &str, Vec<f32>)],
) -> Vec<i64> {
    let mut tx = project_scope(pool, project).await.expect("scope");
    sqlx::query("SET LOCAL ROLE assistant_app")
        .execute(&mut *tx)
        .await
        .expect("role");
    let site: i64 = sqlx::query_scalar("INSERT INTO sites (organization, project, source, visibility) VALUES ('hel', $1, $2, 'public') RETURNING id")
        .bind(project).bind(source).fetch_one(&mut *tx).await.expect("site");
    let page: i64 = sqlx::query_scalar(
        "INSERT INTO pages (site_id, project, url, depth) VALUES ($1, $2, $3, 0) RETURNING id",
    )
    .bind(site)
    .bind(project)
    .bind(format!("https://{source}.example/"))
    .fetch_one(&mut *tx)
    .await
    .expect("page");
    let mut ids = Vec::new();
    for (ordinal, (text, lang, visibility, vector)) in chunks.iter().enumerate() {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO chunks (site_id, project, page_id, ordinal, url, text, lang, visibility, embedding) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::vector) RETURNING id",
        )
        .bind(site).bind(project).bind(page).bind(ordinal as i32)
        .bind(format!("https://{source}.example/#{ordinal}")).bind(*text).bind(*lang).bind(*visibility)
        .bind(assistant::vector_literal(vector).expect("vector"))
        .fetch_one(&mut *tx).await.expect("chunk");
        ids.push(id);
    }
    tx.commit().await.expect("commit");
    ids
}

async fn search(
    pool: &PgPool,
    project: &str,
    text: &str,
    query: &[f32],
    sources: &[&str],
    public_only: bool,
) -> Vec<i64> {
    let sources: Vec<String> = sources.iter().map(|s| (*s).to_owned()).collect();
    let mut tx = project_scope(pool, project).await.expect("scope");
    sqlx::query("SET LOCAL ROLE assistant_app")
        .execute(&mut *tx)
        .await
        .expect("role");
    let hits = hybrid_search(
        &mut tx,
        &Search {
            text,
            embedding: query,
            sources: &sources,
            public_only,
            limit: 10,
        },
    )
    .await
    .expect("search");
    tx.rollback().await.expect("rollback");
    hits.iter().map(|hit| hit.chunk).collect()
}

#[tokio::test]
async fn the_migrations_go_down_and_up_again() {
    let (admin, pool, name) = database("updown").await;
    MIGRATOR.undo(&pool, 0).await.expect("down");
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_tables WHERE schemaname = 'public' AND tablename <> '_sqlx_migrations'")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(left, 0, "down leaves no table behind");
    MIGRATOR.run(&pool).await.expect("up again");
    let extensions: Vec<String> =
        sqlx::query_scalar("SELECT extname::text FROM pg_extension ORDER BY 1")
            .fetch_all(&pool)
            .await
            .expect("extensions");
    assert!(
        extensions.contains(&"vector".to_owned()) && extensions.contains(&"unaccent".to_owned()),
        "{extensions:?}"
    );
    drop_database(admin, pool, &name).await;
}

/// The fixture order: a passage both rankings put first, then one each ranking has.
#[tokio::test]
async fn reciprocal_rank_fusion_puts_what_both_rankings_find_first() {
    let (admin, pool, name) = database("rrf").await;
    let ids = site(
        &pool,
        "helsinki",
        "hel-web",
        &[
            (
                "City bike stations open in April",
                "en",
                "public",
                embedding(0, None),
            ),
            (
                "Bike lanes are repaired this week",
                "en",
                "public",
                embedding(1, None),
            ),
            (
                "Weather for cyclists today",
                "en",
                "public",
                embedding(0, Some((1, 0.1))),
            ),
            ("Parking zones and fees", "en", "public", embedding(2, None)),
        ],
    )
    .await;
    let order = search(
        &pool,
        "helsinki",
        "bikes",
        &embedding(0, None),
        &["hel-web"],
        false,
    )
    .await;
    // A: lexical 1 + semantic 1; B: lexical 2 + semantic 3; C: semantic 2; D: semantic 4.
    assert_eq!(order, vec![ids[0], ids[1], ids[2], ids[3]]);
    drop_database(admin, pool, &name).await;
}

#[tokio::test]
async fn slovak_without_diacritics_finds_slovak_with_them() {
    let (admin, pool, name) = database("unaccent").await;
    let ids = site(
        &pool,
        "banskabystrica",
        "bb-web",
        &[
            (
                "Cyklotrasy v Banskej Bystrici a okolí",
                "sk",
                "public",
                embedding(5, None),
            ),
            (
                "Odvoz odpadu podľa harmonogramu",
                "sk",
                "public",
                embedding(6, None),
            ),
        ],
    )
    .await;
    let order = search(
        &pool,
        "banskabystrica",
        "cyklotrasy banskej bystrici okoli",
        &embedding(9, None),
        &["bb-web"],
        false,
    )
    .await;
    assert_eq!(order.first(), Some(&ids[0]), "{order:?}");
    drop_database(admin, pool, &name).await;
}

#[tokio::test]
async fn a_public_channel_reads_public_chunks_of_its_own_sources_only() {
    let (admin, pool, name) = database("visibility").await;
    let public = site(
        &pool,
        "helsinki",
        "hel-web",
        &[("Library opening hours", "en", "public", embedding(0, None))],
    )
    .await;
    let internal = site(
        &pool,
        "helsinki",
        "hel-intranet",
        &[("Library staff rota", "en", "internal", embedding(0, None))],
    )
    .await;
    let other = site(
        &pool,
        "helsinki",
        "hel-other",
        &[("Library news", "en", "public", embedding(0, None))],
    )
    .await;

    let found = search(
        &pool,
        "helsinki",
        "library",
        &embedding(0, None),
        &["hel-web", "hel-intranet"],
        true,
    )
    .await;
    assert_eq!(
        found, public,
        "public only, and never a source the deployment does not name"
    );
    let found = search(
        &pool,
        "helsinki",
        "library",
        &embedding(0, None),
        &["hel-web", "hel-intranet"],
        false,
    )
    .await;
    assert!(
        found.contains(&internal[0]) && !found.contains(&other[0]),
        "{found:?}"
    );
    drop_database(admin, pool, &name).await;
}

/// T-3325: a citation is named by its page's title and a live-data citation by the page about
/// its Endpoint, from the deployment's own sources, public ones alone on a public channel, and
/// never from another project.
#[tokio::test]
async fn a_citation_is_named_from_the_deployments_own_pages() {
    let (admin, pool, name) = database("titles").await;
    let v = || embedding(0, None);
    site(
        &pool,
        "helsinki",
        "hel-web",
        &[
            (
                "# Helsinki events\n\nWhat happens in the city.",
                "en",
                "public",
                v(),
            ),
            ("A passage with no heading.", "en", "public", v()),
        ],
    )
    .await;
    site(
        &pool,
        "helsinki",
        "hel-catalogue",
        &[(
            "# Events of Helsinki\n\nEndpoint events. Address: https://gw.example/api/endpoint/helsinki-events",
            "en",
            "public",
            v(),
        )],
    )
    .await;
    site(
        &pool,
        "helsinki",
        "hel-intranet",
        &[(
            "# Staff plans\n\nAddress: https://gw.example/api/endpoint/helsinki-staff",
            "en",
            "internal",
            v(),
        )],
    )
    .await;
    site(
        &pool,
        "praha",
        "hel-catalogue",
        &[(
            "# Prague's page\n\nAddress: https://gw.example/api/endpoint/praha-odpad",
            "en",
            "public",
            v(),
        )],
    )
    .await;
    let sources: Vec<String> = ["hel-web", "hel-catalogue", "hel-intranet"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let urls: Vec<String> = [
        "https://hel-web.example/#0",
        "https://hel-web.example/#1",
        "https://hel-catalogue.example/#0",
        "https://hel-intranet.example/#0",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let mut tx = project_scope(&pool, "helsinki").await.expect("scope");
    sqlx::query("SET LOCAL ROLE assistant_app")
        .execute(&mut *tx)
        .await
        .expect("role");

    let public = titles(&mut tx, &urls, &sources, true)
        .await
        .expect("titles");
    assert_eq!(public.len(), 2, "{public:?}");
    assert_eq!(public["https://hel-web.example/#0"], "Helsinki events");
    assert_eq!(
        public["https://hel-catalogue.example/#0"],
        "Events of Helsinki"
    );
    let all = titles(&mut tx, &urls, &sources, false)
        .await
        .expect("titles");
    assert_eq!(all["https://hel-intranet.example/#0"], "Staff plans");
    let web_only = titles(&mut tx, &urls, &sources[..1], false)
        .await
        .expect("titles");
    assert_eq!(
        web_only.len(),
        1,
        "a source the deployment does not name is never read"
    );

    let found = endpoint_page(&mut tx, "helsinki-events", &sources, true)
        .await
        .expect("page");
    assert_eq!(found.as_deref(), Some("https://hel-catalogue.example/#0"));
    let prefix = endpoint_page(&mut tx, "helsinki", &sources, true)
        .await
        .expect("page");
    assert_eq!(
        prefix, None,
        "a slug is matched whole, never as the start of another"
    );
    let hidden = endpoint_page(&mut tx, "helsinki-staff", &sources, true)
        .await
        .expect("page");
    assert_eq!(
        hidden, None,
        "an internal page is never named on a public channel"
    );
    let staff = endpoint_page(&mut tx, "helsinki-staff", &sources, false)
        .await
        .expect("page");
    assert_eq!(staff.as_deref(), Some("https://hel-intranet.example/#0"));
    let other = endpoint_page(&mut tx, "praha-odpad", &sources, false)
        .await
        .expect("page");
    assert_eq!(other, None, "another project's page is never read");
    let odd = endpoint_page(&mut tx, "x|.*", &sources, false)
        .await
        .expect("page");
    assert_eq!(odd, None, "a slug that is no DNS label is never looked up");
    tx.rollback().await.expect("rollback");
    drop_database(admin, pool, &name).await;
}

#[tokio::test]
async fn one_project_never_reads_or_writes_another() {
    let (admin, pool, name) = database("rls").await;
    site(
        &pool,
        "helsinki",
        "web",
        &[("Helsinki only", "en", "public", embedding(0, None))],
    )
    .await;
    let praha = site(
        &pool,
        "praha",
        "web",
        &[("Praha only", "en", "public", embedding(0, None))],
    )
    .await;

    // The same source name in two projects: each scope sees its own.
    assert_eq!(
        search(&pool, "praha", "only", &embedding(0, None), &["web"], false).await,
        praha
    );

    // No scope at all: nothing to read and nothing written.
    let mut conn = pool.acquire().await.expect("conn");
    sqlx::query("SET ROLE assistant_app")
        .execute(&mut *conn)
        .await
        .expect("role");
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks")
        .fetch_one(&mut *conn)
        .await
        .expect("count");
    assert_eq!(seen, 0, "a session that names no project reads nothing");
    let refused = sqlx::query("INSERT INTO sites (organization, project, source, visibility) VALUES ('hel', 'praha', 'x', 'public')")
        .execute(&mut *conn).await;
    assert!(refused.is_err(), "and writes nothing");
    sqlx::query("RESET ROLE")
        .execute(&mut *conn)
        .await
        .expect("reset");
    drop(conn);

    // In helsinki's scope, a write into praha is refused by the policy's WITH CHECK.
    let mut tx = project_scope(&pool, "helsinki").await.expect("scope");
    sqlx::query("SET LOCAL ROLE assistant_app")
        .execute(&mut *tx)
        .await
        .expect("role");
    let crossed = sqlx::query("INSERT INTO sites (organization, project, source, visibility) VALUES ('hel', 'praha', 'y', 'public')")
        .execute(&mut *tx).await;
    assert!(crossed.is_err(), "a scope writes its own project only");
    drop(tx);
    drop_database(admin, pool, &name).await;
}

#[tokio::test]
async fn a_scope_refuses_a_name_that_is_not_a_project() {
    let (admin, pool, name) = database("scopename").await;
    for bad in ["", "Praha", "x' OR '1'='1"] {
        assert!(project_scope(&pool, bad).await.is_err(), "{bad}");
    }
    drop_database(admin, pool, &name).await;
}
