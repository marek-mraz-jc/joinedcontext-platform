//! The test databases' app role is created safely from test binaries running at once (T-3191):
//! ci-full ran several binaries in parallel and three of them lost the race to create the
//! server-wide `assistant_app`, failing on the catalogue's unique index.

#[path = "common/db.rs"]
mod db;

use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn one_role_created_from_many_connections_at_once_is_created_once_and_nobody_fails() {
    let url = std::env::var("JC_ASSISTANT_TEST_DATABASE_URL")
        .expect("set JC_ASSISTANT_TEST_DATABASE_URL (see store_tests.rs)");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("the test server answers");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .subsec_nanos();
    let role: String = format!("race_{nanos}")
        .chars()
        .map(|c| {
            if c.is_ascii_digit() {
                (b'a' + (c as u8 - b'0')) as char
            } else {
                c
            }
        })
        .collect();
    let attempts = (0..8).map(|_| db::ensure_role(&pool, &role));
    for outcome in futures_util::future::join_all(attempts).await {
        outcome.expect("a racing creation is no failure");
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_roles WHERE rolname = $1")
        .bind(&role)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count, 1);
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP ROLE {role}")))
        .execute(&pool)
        .await
        .expect("drop the role");
}
