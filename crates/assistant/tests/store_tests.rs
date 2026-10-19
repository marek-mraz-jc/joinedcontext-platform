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
    dataset_page, dataset_page_projects, endpoint_page, hybrid_search, project_scope,
    search_projects, sources_by_project, titles, Search, DIMENSIONS, MIGRATOR,
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
    tx.rollback().await.expect("rollback");

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
