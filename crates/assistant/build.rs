//! `sqlx::migrate!` embeds `migrations/` at build time; without this a new migration file does
//! not rebuild the crate and the binary ships the old set (sqlx's documented remedy).

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
