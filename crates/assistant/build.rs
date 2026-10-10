//! `sqlx::migrate!` embeds `migrations/` at build time; without this a new migration file does
//! not rebuild the crate and the binary ships the old set (sqlx's documented remedy). The User
//! Guide in `guide/` is embedded the same way (`guide::PAGES`, AG-118): one entry per `NN-*.md`.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=guide");
    let dir = Path::new(&std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo")).join("guide");
    let mut pages: Vec<String> = std::fs::read_dir(&dir)
        .expect("guide/ ships with the crate (scripts/sync-user-guide.sh)")
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".md"))
        .collect();
    pages.sort();
    let entries: Vec<String> = pages
        .iter()
        .map(|name| {
            let path = dir.join(name);
            format!("({name:?}, include_str!({:?}))", path.display().to_string())
        })
        .collect();
    let out = Path::new(&std::env::var("OUT_DIR").expect("set by cargo")).join("guide_pages.rs");
    std::fs::write(out, format!("&[{}]", entries.join(", "))).expect("OUT_DIR is writable");
}
