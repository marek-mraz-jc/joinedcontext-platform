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
    dataset_page, dataset_page_projects, dataset_reads_projects, endpoint_page, fill_terms,
    held_page_projects, hybrid_search, project_scope, rows_dataset_projects, search_projects,
    sources_by_project, titles, Search, DIMENSIONS, MIGRATOR,
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
        "INSERT INTO pages (site_id, project, url, depth, modified, fetched_at) \
         VALUES ($1, $2, $3, 0, '2026-09-30T10:12:00+02:00', '2026-10-19T03:12:40Z') RETURNING id",
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
            lang: None,
        },
    )
    .await
    .expect("search");
    tx.rollback().await.expect("rollback");
    // T-4749: every passage carries its page's dates, in UTC; the one the page never stated is none.
    for hit in &hits {
        assert_eq!(hit.dates.modified.as_deref(), Some("2026-09-30T08:12:00Z"));
        assert_eq!(hit.dates.crawled.as_deref(), Some("2026-10-19T03:12:40Z"));
        assert_eq!(hit.dates.published, None);
    }
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

    // T-4366: the catalogue's page of an Endpoint is cited by its schema, a JSON file to a
    // visitor; the dataset page that names the same Endpoint, indexed later, is cited instead.
    let schema = site(
        &pool,
        "helsinki",
        "hel-od",
        &[(
            "# Air\n\nAddress: https://gw.example/api/endpoint/od-air",
            "en",
            "public",
            v(),
        )],
    )
    .await;
    sqlx::query("UPDATE chunks SET url = 'https://gw.example/api/endpoint/od-air/schema/index.json' WHERE id = $1")
        .bind(schema[0])
        .execute(&pool)
        .await
        .expect("the schema page's url");
    site(
        &pool,
        "helsinki",
        "hel-ckan",
        &[(
            "# Air quality\n\nReadings (API) (https://gw.example/api/endpoint/od-air/ngsi-ld/v1/entities?type=A)",
            "en",
            "public",
            v(),
        )],
    )
    .await;
    let mut tx = project_scope(&pool, "helsinki").await.expect("scope");
    sqlx::query("SET LOCAL ROLE assistant_app")
        .execute(&mut *tx)
        .await
        .expect("role");
    let both = ["hel-od".to_owned(), "hel-ckan".to_owned()];
    let dataset = endpoint_page(&mut tx, "od-air", &both, true)
        .await
        .expect("page");
    assert_eq!(dataset.as_deref(), Some("https://hel-ckan.example/#0"));
    let alone = endpoint_page(&mut tx, "od-air", &both[..1], true)
        .await
        .expect("page");
    assert_eq!(
        alone.as_deref(),
        Some("https://gw.example/api/endpoint/od-air/schema/index.json"),
        "with no dataset page the Endpoint's own page is still named"
    );
    tx.rollback().await.expect("rollback");
    // T-4729: a CKAN dataset page names the Endpoint before any other page that mentions it, an
    // older one included.
    site(
        &pool,
        "helsinki",
        "hel-blog",
        &[(
            "# Our air data

Read it at https://gw.example/api/endpoint/od-air",
            "en",
            "public",
            v(),
        )],
    )
    .await;
    let ckan = site(
        &pool,
        "helsinki",
        "hel-ckan2",
        &[(
            "# Air quality\n\nReadings (API) (https://gw.example/api/endpoint/od-air/ngsi-ld/v1/entities?type=A)",
            "en",
            "public",
            v(),
        )],
    )
    .await;
    sqlx::query(
        "UPDATE chunks SET url = 'https://opendata.example/dataset/air-quality' WHERE id = $1",
    )
    .bind(ckan[0])
    .execute(&pool)
    .await
    .expect("the dataset page's url");
    let mut tx = project_scope(&pool, "helsinki").await.expect("scope");
    sqlx::query("SET LOCAL ROLE assistant_app")
        .execute(&mut *tx)
        .await
        .expect("role");
    let blog_and_ckan = ["hel-blog".to_owned(), "hel-ckan2".to_owned()];
    let first = endpoint_page(&mut tx, "od-air", &blog_and_ckan, true)
        .await
        .expect("page");
    assert_eq!(
        first.as_deref(),
        Some("https://opendata.example/dataset/air-quality")
    );
    // T-4625: rows of type A read through another Endpoint of the space cite the dataset page
    // whose NGSI-LD example serves A from a sibling; another type or no sibling finds nothing.
    let siblings = ["od-all".to_owned(), "od-air".to_owned()];
    let typed = dataset_page(&mut tx, &siblings, "A", &both, true)
        .await
        .expect("page");
    assert_eq!(typed.as_deref(), Some("https://hel-ckan.example/#0"));
    let projects = [("helsinki".to_owned(), both.to_vec())];
    for (slugs, kind, found) in [
        (&siblings[..], "A", Some("https://hel-ckan.example/#0")),
        (&siblings[..], "B", None),
        (&siblings[..1], "A", None),
    ] {
        let page = dataset_page_projects(&pool, slugs, kind, &projects, true)
            .await
            .expect("page");
        assert_eq!(page.as_deref(), found, "{slugs:?} {kind}");
    }
    // T-4793: a dataset page an entity names is cited only when the sources hold it.
    for (url, held) in [
        ("https://hel-ckan.example/#0", true),
        ("https://elsewhere.example/#0", false),
    ] {
        let found = held_page_projects(&pool, url, &projects, true)
            .await
            .expect("held");
        assert_eq!(found, held, "{url}");
    }
    tx.rollback().await.expect("rollback");

    // T-5929: a CKAN page that reads its rows of a type from an Endpoint by isPartOf narrows a
    // read of that Endpoint and type to them; another Endpoint, another type, a page whose
    // address names no isPartOf (od-air's above) or a source the deployment does not name never.
    let lekarne = site(
        &pool,
        "helsinki",
        "hel-ckan3",
        &[(
            "# Zoznam lekární v BBK\n\nNGSI-LD API (PointOfInterest) (https://gw.example/api/endpoint/od-all/ngsi-ld/v1/entities?type=PointOfInterest&q=isPartOf%3D%3D%22https%3A%2F%2Fopendata.example%2Fdataset%2Flekarne%22)",
            "sk",
            "public",
            v(),
        )],
    )
    .await;
    sqlx::query("UPDATE chunks SET url = 'https://opendata.example/dataset/lekarne' WHERE id = $1")
        .bind(lekarne[0])
        .execute(&pool)
        .await
        .expect("the dataset page's url");
    let named = [(
        "helsinki".to_owned(),
        vec!["hel-ckan3".to_owned(), "hel-ckan2".to_owned()],
    )];
    let other = [("helsinki".to_owned(), vec!["hel-blog".to_owned()])];
    for (url, slug, kind, sources, reads) in [
        (
            "https://opendata.example/dataset/lekarne",
            "od-all",
            "PointOfInterest",
            &named,
            true,
        ),
        (
            "https://opendata.example/dataset/lekarne",
            "od-air",
            "PointOfInterest",
            &named,
            false,
        ),
        (
            "https://opendata.example/dataset/lekarne",
            "od-all",
            "Point",
            &named,
            false,
        ),
        (
            "https://opendata.example/dataset/lekarne",
            "od-all",
            "Point_",
            &named,
            false,
        ),
        (
            "https://opendata.example/dataset/air-quality",
            "od-air",
            "A",
            &named,
            false,
        ),
        (
            "https://opendata.example/dataset/lekarne",
            "od-all",
            "PointOfInterest",
            &other,
            false,
        ),
    ] {
        let found = dataset_reads_projects(&pool, url, slug, kind, sources, true)
            .await
            .expect("reads");
        assert_eq!(found, reads, "{url} {slug} {kind}");
    }

    // T-4711: a dataset renamed in the catalogue leaves its old page in the index; the page
    // indexed later for the same Endpoint and type is the one cited, never the stale 404.
    site(
        &pool,
        "helsinki",
        "hel-ckan-new",
        &[(
            "# Air quality, renamed\n\nReadings (API) (https://gw.example/api/endpoint/od-air/ngsi-ld/v1/entities?type=A)",
            "en",
            "public",
            v(),
        )],
    )
    .await;
    let mut tx = project_scope(&pool, "helsinki").await.expect("scope");
    sqlx::query("SET LOCAL ROLE assistant_app")
        .execute(&mut *tx)
        .await
        .expect("role");
    let three = ["hel-ckan".to_owned(), "hel-ckan-new".to_owned()];
    let newest = dataset_page(&mut tx, &siblings, "A", &three, true)
        .await
        .expect("page");
    assert_eq!(newest.as_deref(), Some("https://hel-ckan-new.example/#0"));
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

/// T-4281: a deployment's `{project}/{source}` sources are searched in that project's own scope
/// and merged; a project it names no source of is never read, even through a source of the same
/// name. The search runs as a login role of the service's own, so row-level security holds.
#[tokio::test]
async fn sources_of_another_project_are_searched_in_its_own_scope() {
    let (admin, pool, name) = database("crossproject").await;
    let city = site(
        &pool,
        "banskabystrica",
        "web",
        &[("Zber odpadu v meste", "sk", "public", embedding(0, None))],
    )
    .await;
    let region = site(
        &pool,
        "bbsk",
        "web",
        &[(
            "Zber odpadu v kraji",
            "sk",
            "public",
            embedding(0, Some((1, 0.2))),
        )],
    )
    .await;
    let other = site(
        &pool,
        "zilina",
        "web",
        &[(
            "Zber odpadu v Žiline",
            "sk",
            "public",
            embedding(0, Some((2, 0.1))),
        )],
    )
    .await;
    sqlx::query("DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'assistant_login') THEN CREATE ROLE assistant_login LOGIN PASSWORD 'pw' IN ROLE assistant_app; END IF; END $$")
        .execute(&pool).await.expect("login role");
    let url = std::env::var("JC_ASSISTANT_TEST_DATABASE_URL").expect("url");
    let options: PgConnectOptions = url.parse().expect("url parses");
    let app = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(
            options
                .database(&name)
                .username("assistant_login")
                .password("pw"),
        )
        .await
        .expect("connect as the service");

    let query = embedding(0, None);
    let found = |sources: Vec<(String, Vec<String>)>| {
        let app = app.clone();
        let query = query.clone();
        async move {
            let hits = search_projects(
                &app,
                &sources,
                &Search {
                    text: "zber odpadu",
                    embedding: &query,
                    sources: &[],
                    public_only: true,
                    limit: 10,
                    lang: None,
                },
            )
            .await
            .expect("search");
            hits.iter()
                .map(|h| (h.chunk, h.site.clone()))
                .collect::<Vec<_>>()
        }
    };
    let chunks = |hits: Vec<(i64, String)>| hits.into_iter().map(|(c, _)| c).collect::<Vec<_>>();
    let names = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let sited = found(sources_by_project(
        "banskabystrica",
        &names(&["web", "bbsk/web"]),
    ))
    .await;
    // T-4680: each passage names the source it is from, its project's own.
    assert!(
        sited.contains(&(city[0], "banskabystrica/web".to_owned()))
            && sited.contains(&(region[0], "bbsk/web".to_owned())),
        "{sited:?}"
    );
    let both = chunks(sited);
    assert_eq!(both.len(), 2, "{both:?}");
    assert!(
        both.contains(&city[0]) && both.contains(&region[0]),
        "{both:?}"
    );
    assert!(!both.contains(&other[0]));
    // The closer vector ranks first across the two projects' rankings.
    assert_eq!(both[0], city[0]);

    let own = chunks(found(sources_by_project("banskabystrica", &names(&["web"]))).await);
    assert_eq!(own, city);
    let theirs = chunks(found(sources_by_project("banskabystrica", &names(&["bbsk/web"]))).await);
    assert_eq!(theirs, region);
    app.close().await;
    drop_database(admin, pool, &name).await;
}

/// T-4370: a passage on a news page ranks below the same passage on a page beside the news,
/// though both rankings put the news one first.
#[tokio::test]
async fn a_news_page_ranks_below_the_same_passage_elsewhere() {
    let (admin, pool, name) = database("news").await;
    let text = "Plaváreň je otvorená denne od 6:00 do 21:00";
    let ids = site(
        &pool,
        "bb",
        "bb-web",
        &[
            (text, "sk", "public", embedding(0, None)),
            (text, "sk", "public", embedding(0, None)),
        ],
    )
    .await;
    let mut tx = project_scope(&pool, "bb").await.expect("scope");
    for (id, url) in [
        (
            ids[0],
            "https://bb.example/aktuality/kategorie-aktualit/covid-19/",
        ),
        (ids[1], "https://bb.example/sport/plavaren"),
    ] {
        sqlx::query("UPDATE chunks SET url = $2 WHERE id = $1")
            .bind(id)
            .bind(url)
            .execute(&mut *tx)
            .await
            .expect("url");
    }
    tx.commit().await.expect("commit");
    let order = search(
        &pool,
        "bb",
        "plaváreň otvorená",
        &embedding(0, None),
        &["bb-web"],
        false,
    )
    .await;
    assert_eq!(order, vec![ids[1], ids[0]]);
    drop_database(admin, pool, &name).await;
}

/// T-4747: an Endpoint that serves many types is named by every dataset page that publishes one
/// of them; none of those is "its" page, so none is lent to a citation of another type (bike
/// routes were cited as "Členovia samosprávy"). An Endpoint with one dataset keeps its page.
#[tokio::test]
async fn an_endpoint_named_by_two_dataset_pages_lends_neither() {
    let (admin, pool, name) = database("endpoint_shared").await;
    let v = || embedding(0, None);
    let pages = site(
        &pool,
        "bbsk",
        "bbsk-ckan",
        &[
            (
                "# Členovia samosprávy\n\nNGSI-LD (https://gw.example/api/endpoint/bbsk-all/ngsi-ld/v1/entities?type=Person)",
                "sk",
                "public",
                v(),
            ),
            (
                "# Stredné školy\n\nNGSI-LD (https://gw.example/api/endpoint/bbsk-all/ngsi-ld/v1/entities?type=School)",
                "sk",
                "public",
                v(),
            ),
            (
                "# Ovzdušie\n\nNGSI-LD (https://gw.example/api/endpoint/bbsk-air/ngsi-ld/v1/entities?type=AirQualityObserved)",
                "sk",
                "public",
                v(),
            ),
        ],
    )
    .await;
    for (id, dataset) in pages.iter().zip(["clenovia", "stredne-skoly", "ovzdusie"]) {
        sqlx::query("UPDATE chunks SET url = $2 WHERE id = $1")
            .bind(id)
            .bind(format!("https://opendata.example/dataset/{dataset}"))
            .execute(&pool)
            .await
            .expect("the dataset page's url");
    }
    let mut tx = project_scope(&pool, "bbsk").await.expect("scope");
    sqlx::query("SET LOCAL ROLE assistant_app")
        .execute(&mut *tx)
        .await
        .expect("role");
    let ckan = ["bbsk-ckan".to_owned()];
    let shared = endpoint_page(&mut tx, "bbsk-all", &ckan, true)
        .await
        .expect("page");
    assert_eq!(
        shared, None,
        "no dataset page of a shared Endpoint is its page"
    );
    let own = endpoint_page(&mut tx, "bbsk-air", &ckan, true)
        .await
        .expect("page");
    assert_eq!(
        own.as_deref(),
        Some("https://opendata.example/dataset/ovzdusie"),
        "an Endpoint with one dataset keeps it"
    );
    let typed = dataset_page(&mut tx, &["bbsk-all".to_owned()], "School", &ckan, true)
        .await
        .expect("page");
    assert_eq!(
        typed.as_deref(),
        Some("https://opendata.example/dataset/stredne-skoly"),
        "the type finds its own page"
    );
    tx.rollback().await.expect("rollback");
    drop_database(admin, pool, &name).await;
}

/// T-4802, prod bbsk-demo 2026-10-20: the assistant reads Bridge rows through the multi-type
/// "Registre kraja" (lws6dd…), which no dataset names; the dataset bbsk-mosty publishes the same
/// Bridge entities of the space through its own Endpoint (omto6q…). Its page names the type the
/// publisher now takes from the Endpoint's type list: Bridge rows cite it, a type no dataset
/// publishes cites none, and a type the page does not name (the guide's old AdministrativeArea)
/// never borrows it.
#[tokio::test]
async fn rows_of_an_unpublished_endpoint_cite_the_dataset_of_its_sibling() {
    let (admin, pool, name) = database("sibling_dataset").await;
    let v = || embedding(0, None);
    let api = "https://api.example.org/api/endpoint/omto6qk5sfn5ft7zl5pdiaxunj2ljnae";
    let pages = site(
        &pool,
        "bbsk",
        "bbsk-ckan",
        &[(
            &format!(
                "# Mosty — Banskobystrický kraj\n\nNGSI-LD API ({api}/ngsi-ld/v1/) \
                 ({api}/ngsi-ld/v1/entities?type=Bridge&limit=10)\n\nOData v4 ({api}/odata/v4/)"
            ),
            "sk",
            "public",
            v(),
        )],
    )
    .await;
    sqlx::query(
        "UPDATE chunks SET url = 'https://opendata.example/dataset/bbsk-mosty' WHERE id = $1",
    )
    .bind(pages[0])
    .execute(&pool)
    .await
    .expect("the dataset page's url");
    let siblings = [
        "lws6dd7o56veegwxsjtvak3goeta4mfn".to_owned(),
        "omto6qk5sfn5ft7zl5pdiaxunj2ljnae".to_owned(),
    ];
    let projects = [("bbsk".to_owned(), vec!["bbsk-ckan".to_owned()])];
    for (kind, found) in [
        (
            "Bridge",
            Some("https://opendata.example/dataset/bbsk-mosty"),
        ),
        ("PointOfInterest", None),
        ("AdministrativeArea", None),
    ] {
        let page = dataset_page_projects(&pool, &siblings, kind, &projects, true)
            .await
            .expect("page");
        assert_eq!(page.as_deref(), found, "{kind}");
    }
    // The space's other Endpoint alone does not reach the sibling's dataset.
    let alone = dataset_page_projects(&pool, &siblings[..1], "Bridge", &projects, true)
        .await
        .expect("page");
    assert_eq!(alone, None);
    drop_database(admin, pool, &name).await;
}

/// T-4802, the owner's sweep: every public Endpoint x type of prod (deployment
/// scripts/smoke-source-links.py, smartcity 2026-10-20), with the CKAN pages as the assistant
/// indexes them. Each page of an Endpoint's one dataset carries the entity sets jcctl's OData text
/// now names (publish/ckan.rs `entity_sets`); a harvested page of a shared Endpoint stays as it
/// is. Rows of each (Endpoint, type) cite one of the datasets that publish their space and type,
/// the way `dataset_of` looks them up, and none when no dataset does.
#[tokio::test]
async fn every_public_endpoint_and_type_of_prod_cites_its_dataset() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/sweep/smoke-source-links-smartcity-20261020.json"
    ))
    .expect("the sweep's fixture");
    let (admin, pool, name) = database("sweep_links").await;
    let owners = fixture["owners"].as_object().expect("owners");
    let served = &fixture["served"];
    let pages: Vec<(String, String)> = fixture["pages"]
        .as_array()
        .expect("pages")
        .iter()
        .map(|page| {
            let url = page["url"].as_str().expect("url").to_owned();
            let mut text = page["text"].as_str().expect("text").to_owned();
            let sole = owners.iter().find(|(_, urls)| {
                urls.as_array()
                    .is_some_and(|urls| urls.len() == 1 && urls[0] == url.as_str())
            });
            if let Some((slug, _)) = sole {
                let base =
                    format!("https://api.smartcity.joinedcontext.com/api/endpoint/{slug}/odata/v4");
                let sets: Vec<String> = served[slug.as_str()]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .map(|kind| format!("{base}/{kind}"))
                    .collect();
                if !sets.is_empty() {
                    text.push_str(&format!("\nIts entity sets: {}.", sets.join(", ")));
                }
            }
            (url, text)
        })
        .collect();
    let chunks: Vec<(&str, &str, &str, Vec<f32>)> = pages
        .iter()
        .map(|(_, text)| (text.as_str(), "sk", "public", embedding(0, None)))
        .collect();
    let ids = site(&pool, "smartcity", "ckan", &chunks).await;
    for (id, (url, _)) in ids.iter().zip(&pages) {
        sqlx::query("UPDATE chunks SET url = $2 WHERE id = $1")
            .bind(id)
            .bind(url)
            .execute(&pool)
            .await
            .expect("the dataset page's url");
    }
    let projects = [("smartcity".to_owned(), vec!["ckan".to_owned()])];
    let rows = fixture["rows"].as_array().expect("rows");
    assert!(rows.len() > 100, "the sweep covers prod");
    let mut misses = Vec::new();
    for row in rows {
        let slug = row["endpoint"].as_str().expect("endpoint");
        let kind = row["type"].as_str().expect("type");
        let siblings: Vec<String> = row["siblings"]
            .as_array()
            .expect("siblings")
            .iter()
            .filter_map(|s| s.as_str().map(str::to_owned))
            .collect();
        let expected = row["expected"].as_str().expect("expected");
        let found = rows_dataset_projects(&pool, slug, &siblings, kind, &projects, true)
            .await
            .expect("page");
        let good = match found.as_deref() {
            None => expected == "-",
            Some(page) => expected.split(',').any(|url| url == page),
        };
        if !good {
            misses.push(format!("{slug} {kind}: {found:?}, expected {expected}"));
        }
    }
    assert!(misses.is_empty(), "{}", misses.join("\n"));
    drop_database(admin, pool, &name).await;
}

/// T-5773: the semantic arm walks the HNSW index instead of sorting every chunk of the scope by
/// distance (23.5 s on prod at 130k chunks), and the walk goes on past `ef_search` until the
/// scope's own chunks are found: a source of 2 chunks beside 400 nearer ones of another source
/// still gets them. Sequential and bitmap scans and sorts are off so a table this small plans as
/// the large one does.
#[tokio::test]
async fn the_semantic_arm_walks_the_vector_index_past_other_sources_chunks() {
    let (admin, pool, name) = database("knn").await;
    let near: Vec<(String, Vec<f32>)> = (0..400)
        .map(|i| {
            (
                format!("Pier {i}"),
                embedding(0, Some((2, i as f32 / 1000.0))),
            )
        })
        .collect();
    let near: Vec<(&str, &str, &str, Vec<f32>)> = near
        .iter()
        .map(|(text, v)| (text.as_str(), "en", "public", v.clone()))
        .collect();
    site(&pool, "helsinki", "hel-big", &near).await;
    let small = site(
        &pool,
        "helsinki",
        "hel-small",
        &[
            (
                "Library hours",
                "en",
                "public",
                embedding(1, Some((0, 0.5))),
            ),
            (
                "Library cards",
                "en",
                "public",
                embedding(1, Some((0, 0.4))),
            ),
        ],
    )
    .await;
    let sources = vec!["hel-small".to_owned()];
    let mut tx = project_scope(&pool, "helsinki").await.expect("scope");
    sqlx::raw_sql(
        "SET LOCAL ROLE assistant_app; SET LOCAL enable_seqscan = off; \
         SET LOCAL enable_bitmapscan = off; SET LOCAL enable_sort = off",
    )
    .execute(&mut *tx)
    .await
    .expect("settings");
    let hits = hybrid_search(
        &mut tx,
        &Search {
            text: "zzzz",
            embedding: &embedding(0, None),
            sources: &sources,
            public_only: true,
            limit: 10,
            lang: None,
        },
    )
    .await
    .expect("search");
    let walked: i64 =
        sqlx::query_scalar("SELECT pg_stat_get_xact_numscans('chunks_embedding'::regclass)")
            .fetch_one(&mut *tx)
            .await
            .expect("index scans");
    tx.rollback().await.expect("rollback");
    assert_eq!(
        hits.iter().map(|hit| hit.chunk).collect::<Vec<_>>(),
        vec![small[0], small[1]]
    );
    assert_eq!(walked, 1, "the semantic arm is served by chunks_embedding");
    drop_database(admin, pool, &name).await;
}

/// A pool on `pool`'s database whose every connection is the app role, so row-level security
/// holds for what it runs as it does for the service.
async fn app_pool(pool: &PgPool) -> PgPool {
    PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::Executor::execute(conn, "SET ROLE assistant_app").await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .expect("connect as the app role")
}

/// T-5799: the lexical arm finds its chunks through `chunk_terms` (a btree whose leakproof text
/// `=` the forced row-level security lets serve the lookup) and never scans the project's chunks,
/// which took 2.6 s on prod at 101k chunks with `@@` evaluated on each. 100k chunks, a sixth of
/// them matching as on prod, as the app role with the generic plan sqlx runs.
#[tokio::test]
async fn the_lexical_arm_looks_chunks_up_by_term_and_never_scans_the_project() {
    let (admin, pool, name) = database("lexical").await;
    site(&pool, "banskabystrica", "bb-web", &[]).await;
    site(&pool, "bbsk", "bbsk-web", &[]).await;
    sqlx::query(
        "INSERT INTO chunks (site_id, project, page_id, ordinal, url, text, lang, visibility) \
         SELECT s.id, s.project, p.id, n, p.url || '#' || n, \
                CASE WHEN n % 6 = 0 THEN 'Kvalita ovzdušia v Banskej Bystrici, meranie ' || n \
                     ELSE 'Rozpočet mesta, zápisnica číslo ' || n || ' zastupiteľstvo' END, \
                'sk', 'public' \
         FROM sites s JOIN pages p ON p.site_id = s.id, generate_series(1, 100000) n \
         WHERE s.source = 'bb-web' OR (s.source = 'bbsk-web' AND n <= 2000)",
    )
    .execute(&pool)
    .await
    .expect("100k chunks");
    sqlx::raw_sql("ANALYZE chunks; ANALYZE chunk_terms; ANALYZE sites")
        .execute(&pool)
        .await
        .expect("analyze");
    let sources = vec!["bb-web".to_owned()];
    let mut tx = project_scope(&pool, "banskabystrica").await.expect("scope");
    sqlx::raw_sql("SET LOCAL ROLE assistant_app; SET LOCAL plan_cache_mode = force_generic_plan")
        .execute(&mut *tx)
        .await
        .expect("settings");
    let hits = hybrid_search(
        &mut tx,
        &Search {
            text: "Aká je kvalita ovzdušia v Banskej Bystrici?",
            embedding: &embedding(0, None),
            sources: &sources,
            public_only: true,
            limit: 10,
            lang: None,
        },
    )
    .await
    .expect("search");
    let (scanned, fetched, looked_up): (i64, i64, i64) = sqlx::query_as(
        "SELECT pg_stat_get_xact_numscans('chunks'::regclass), \
                pg_stat_get_xact_tuples_fetched('chunks'::regclass), \
                pg_stat_get_xact_numscans('chunk_terms_lookup'::regclass)",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("scans");
    tx.rollback().await.expect("rollback");
    assert_eq!(hits.len(), 10);
    assert!(
        hits.iter()
            .all(|hit| hit.text.starts_with("Kvalita ovzdušia")
                && hit.site == "banskabystrica/bb-web"),
        "{hits:?}"
    );
    assert_eq!(scanned, 0, "no sequential scan of chunks");
    // The 16,667 chunks that hold a term of the question are checked against the scope by id;
    // a read of the project's chunks (by chunks_scope) fetches all 100,000.
    assert!(fetched <= 20_000, "{fetched} chunks read");
    // PostgreSQL 16 counts one scan for each term of the `= ANY` array.
    assert!(
        looked_up >= 1,
        "the lexical arm is served by chunk_terms_lookup"
    );
    drop_database(admin, pool, &name).await;
}

/// The chunks the lexical arm found for `text`, sorted. Every chunk of the fixtures carries an
/// embedding and the semantic arm returns each chunk in scope (fewer than CANDIDATES), so a chunk
/// scores above `1 / (RRF_K + 1)` only when the lexical arm found it too.
async fn by_words(
    pool: &PgPool,
    project: &str,
    text: &str,
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
            embedding: &embedding(0, None),
            sources: &sources,
            public_only,
            limit: 10,
            lang: None,
        },
    )
    .await
    .expect("search");
    tx.rollback().await.expect("rollback");
    let only_semantic = 1.0 / f64::from(assistant::RRF_K + 1);
    let mut ids: Vec<i64> = hits
        .iter()
        .filter(|hit| hit.score > only_semantic + 1e-9)
        .map(|hit| hit.chunk)
        .collect();
    ids.sort_unstable();
    ids
}

/// T-5799: a chunk's terms are its own project's and follow its text, a chunk of another project
/// or a private one never reaches the lexical arm's answer, and a chunk stored before
/// `chunk_terms` existed is still found by its words until [`fill_terms`] gives it terms.
#[tokio::test]
async fn chunk_terms_keep_to_their_project_and_follow_the_text() {
    let (admin, pool, name) = database("terms").await;
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
    let espoo = site(
        &pool,
        "espoo",
        "hel-web",
        &[("Library in Espoo", "en", "public", embedding(0, None))],
    )
    .await;
    let both = ["hel-web", "hel-intranet"];
    let mut helsinki = vec![public[0], internal[0]];
    helsinki.sort_unstable();
    assert_eq!(
        by_words(&pool, "helsinki", "library", &both, true).await,
        public
    );
    assert_eq!(
        by_words(&pool, "helsinki", "library", &both, false).await,
        helsinki
    );

    let app = app_pool(&pool).await;
    let mut tx = project_scope(&app, "helsinki").await.expect("scope");
    let projects: Vec<String> = sqlx::query_scalar("SELECT DISTINCT project FROM chunk_terms")
        .fetch_all(&mut *tx)
        .await
        .expect("projects");
    let foreign: i64 = sqlx::query_scalar("SELECT count(*) FROM chunk_terms WHERE chunk_id = $1")
        .bind(espoo[0])
        .fetch_one(&mut *tx)
        .await
        .expect("espoo's terms");
    tx.rollback().await.expect("rollback");
    assert_eq!(projects, vec!["helsinki".to_owned()]);
    assert_eq!(foreign, 0, "another project's terms are not readable");

    // As before the migration: no terms, every chunk pending.
    sqlx::raw_sql("DELETE FROM chunk_terms; UPDATE chunks SET terms_pending = true")
        .execute(&pool)
        .await
        .expect("pending");
    assert_eq!(
        by_words(&pool, "helsinki", "library", &both, false).await,
        helsinki,
        "a pending chunk is found by its words"
    );
    assert_eq!(
        by_words(&pool, "helsinki", "staff", &both, true).await,
        Vec::<i64>::new()
    );
    assert_eq!(fill_terms(&app, "helsinki", 1000).await.expect("fill"), 2);
    assert_eq!(fill_terms(&app, "helsinki", 1000).await.expect("fill"), 0);
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks WHERE terms_pending")
        .fetch_one(&pool)
        .await
        .expect("pending");
    assert_eq!(pending, 1, "espoo's chunk waits for espoo's own pass");
    assert_eq!(fill_terms(&app, "espoo", 1000).await.expect("fill"), 1);
    assert_eq!(
        by_words(&pool, "helsinki", "library", &both, false).await,
        helsinki
    );
    assert_eq!(
        by_words(&pool, "helsinki", "staff", &both, true).await,
        Vec::<i64>::new()
    );

    let mut tx = project_scope(&app, "helsinki").await.expect("scope");
    sqlx::query("UPDATE chunks SET text = 'Swimming hall' WHERE id = $1")
        .bind(public[0])
        .execute(&mut *tx)
        .await
        .expect("new text");
    tx.commit().await.expect("commit");
    assert_eq!(
        by_words(&pool, "helsinki", "library", &both, true).await,
        Vec::<i64>::new()
    );
    assert_eq!(
        by_words(&pool, "helsinki", "swimming", &both, true).await,
        public
    );
    app.close().await;
    drop_database(admin, pool, &name).await;
}
