//! A fresh test database with the store's migrations (`JC_ASSISTANT_TEST_DATABASE_URL`, see
//! store_tests.rs for the server). A missing variable is a failure that says what to set, never
//! a test that passes by skipping.

use assistant::MIGRATOR;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, Executor, PgPool};

/// A new database with the migrations applied: the admin pool, a pool whose every connection
/// is the non-superuser `assistant_app` (so row-level security holds for it, as for the
/// service's own role), and the database's name.
pub async fn database(test: &str) -> (PgPool, PgPool, String) {
    let url = std::env::var("JC_ASSISTANT_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!("set JC_ASSISTANT_TEST_DATABASE_URL to a PostgreSQL with pgvector whose user may create databases (see store_tests.rs)")
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
    let options = admin_options.database(&name).disable_statement_logging();
    let owner = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .expect("connect to the test database");
    MIGRATOR.run(&owner).await.expect("the migrations apply");
    for statement in [
        "DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'assistant_app') THEN CREATE ROLE assistant_app NOLOGIN; END IF; END $$",
        "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO assistant_app",
        "GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO assistant_app",
    ] {
        sqlx::query(statement).execute(&owner).await.expect(statement);
    }
    owner.close().await;
    let app = PgPoolOptions::new()
        .max_connections(4)
        .after_connect(|conn, _| {
            Box::pin(async move {
                conn.execute("SET ROLE assistant_app").await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .expect("connect as the app role");
    (admin, app, name)
}

pub async fn drop_database(admin: PgPool, pool: PgPool, name: &str) {
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .expect("drop the test database");
}
