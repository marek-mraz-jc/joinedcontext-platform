//! The 10 000-App load test's provisioner (T-3345): one component, made into N different ones,
//! each with its own schema, role and prefix, placed across the shards, the way the reconciler
//! places published Apps. It runs the reconciler's statements from `provision`, as written, so a
//! fleet is provisioned as dev provisions one App.
//!
//! ```sh
//! cargo run --release -p wasm-host --example fleet -- <component.wasm> <migration.sql> <apps> <shards> <out-dir>
//! ```
//!
//! | Variable | Meaning |
//! |---|---|
//! | `FLEET_DB_URL` | the apps database as its owner login, which is also the migrator |
//! | `FLEET_S3_ENDPOINT`, `FLEET_S3_KEY`, `FLEET_S3_SECRET` | the store and a key that may create and write the bucket `apps` |
//!
//! It writes `<out-dir>/shard-<n>.json` (the placement each shard reads) and
//! `<out-dir>/shard-<n>.password` (mode 600, the shard login's password). App `i` is `load-<i>`
//! with id `<i as 16 hex digits>` on shard `i % shards`, in project `t<i % 100>`. Each App's
//! component is the one given plus a custom section holding `i`, so every digest differs and
//! every App is compiled on its own, as N different Apps would be.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;

use sha2::{Digest as _, Sha256};
use sqlx::{AssertSqlSafe, Connection as _, PgConnection};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use wasm_host::provision;
use wasm_host::s3::Bucket;
use wasm_host::source::component_key;

/// Apps provisioned per transaction, and transactions and uploads in flight.
const BATCH: usize = 50;
const DB_WORKERS: usize = 4;
const UPLOADS: usize = 16;

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

/// `base` with a custom section `jc-load` holding `n`: a valid component with a digest of its own.
pub fn variant(base: &[u8], n: u32) -> Vec<u8> {
    let name = b"jc-load";
    let payload = n.to_le_bytes();
    let mut bytes = base.to_vec();
    bytes.push(0);
    bytes.push((1 + name.len() + payload.len()) as u8);
    bytes.push(name.len() as u8);
    bytes.extend_from_slice(name);
    bytes.extend_from_slice(&payload);
    bytes
}

fn id_of(n: u32) -> String {
    format!("{n:016x}")
}

fn password() -> Result<String, String> {
    let mut raw = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut raw))
        .map_err(|err| format!("/dev/urandom: {err}"))?;
    Ok(hex::encode(raw))
}

/// The statements that place App `n`: its roles and schema, its migration as its owner, and one
/// row, so every App holds data as a used App does.
fn app_statements(n: u32, shards: u32, migrator: &str, migration: &str) -> Result<String, String> {
    let id = id_of(n);
    let mut sql = provision::app(&(n % shards).to_string(), &id, migrator)?;
    sql.push(format!("set role {}", provision::owner_of(&id)));
    sql.push(format!("set search_path to app_{id}"));
    sql.push(migration.trim().trim_end_matches(';').to_owned());
    sql.push(format!("insert into notes (body) values ('seed {n}')"));
    sql.push("reset search_path".into());
    sql.push("reset role".into());
    Ok(sql.join(";\n") + ";\n")
}

#[tokio::main]
async fn main() {
    if let Err(why) = run().await {
        eprintln!("fleet: {why}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [component, migration, apps, shards, out] = args.as_slice() else {
        return Err(
            "usage: fleet <component.wasm> <migration.sql> <apps> <shards> <out-dir>".into(),
        );
    };
    let apps: u32 = apps.parse().map_err(|_| "<apps> is a number")?;
    let shards: u32 = shards.parse().map_err(|_| "<shards> is a number")?;
    if apps == 0 || shards == 0 {
        return Err("<apps> and <shards> are at least 1".into());
    }
    let base = std::fs::read(component).map_err(|err| format!("{component}: {err}"))?;
    let migration =
        std::fs::read_to_string(migration).map_err(|err| format!("{migration}: {err}"))?;
    let out = PathBuf::from(out);
    std::fs::create_dir_all(&out).map_err(|err| format!("{}: {err}", out.display()))?;

    let url = env("FLEET_DB_URL")?;
    let migrator = url::Url::parse(&url)
        .map_err(|err| format!("FLEET_DB_URL: {err}"))?
        .username()
        .to_owned();
    let database = url::Url::parse(&url)
        .map_err(|err| format!("FLEET_DB_URL: {err}"))?
        .path()
        .trim_start_matches('/')
        .to_owned();

    // The database and the shard logins, once.
    let mut db = PgConnection::connect(&url)
        .await
        .map_err(|err| format!("the database: {err}"))?;
    let mut once = provision::database(&database);
    for shard in 0..shards {
        let shard = shard.to_string();
        once.extend(provision::shard(&shard)?);
        let secret = password()?;
        once.push(format!("alter role wasm_host_{shard} password '{secret}'"));
        let file = out.join(format!("shard-{shard}.password"));
        std::fs::write(&file, &secret).map_err(|err| format!("{}: {err}", file.display()))?;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
            .map_err(|err| format!("{}: {err}", file.display()))?;
    }
    for statement in once {
        sqlx::raw_sql(AssertSqlSafe(statement))
            .execute(&mut db)
            .await
            .map_err(|err| format!("provisioning the database: {err}"))?;
    }
    drop(db);

    // Every App's schema, in batches, a few connections at once.
    let started = std::time::Instant::now();
    let migration = Arc::new(migration);
    let mut work = JoinSet::new();
    let next = Arc::new(std::sync::atomic::AtomicU32::new(0));
    for _ in 0..DB_WORKERS {
        let (url, migrator, migration, next) = (
            url.clone(),
            migrator.clone(),
            migration.clone(),
            next.clone(),
        );
        work.spawn(async move {
            let mut db = PgConnection::connect(&url)
                .await
                .map_err(|err| format!("the database: {err}"))?;
            loop {
                let from = next.fetch_add(BATCH as u32, std::sync::atomic::Ordering::SeqCst);
                if from >= apps {
                    return Ok::<_, String>(());
                }
                let mut sql = String::from("begin;\n");
                for n in from..(from + BATCH as u32).min(apps) {
                    sql.push_str(&app_statements(n, shards, &migrator, &migration)?);
                }
                sql.push_str("commit;\n");
                sqlx::raw_sql(AssertSqlSafe(sql))
                    .execute(&mut db)
                    .await
                    .map_err(|err| format!("Apps {from}..: {err}"))?;
            }
        });
    }
    while let Some(done) = work.join_next().await {
        done.map_err(|err| err.to_string())??;
    }
    eprintln!("fleet: {apps} schemas in {:?}", started.elapsed());

    // Every App's component and one object under its prefix.
    let started = std::time::Instant::now();
    let bucket = Arc::new(Bucket {
        endpoint: env("FLEET_S3_ENDPOINT")?,
        bucket: "apps".into(),
        region: "us-east-1".into(),
        key_id: env("FLEET_S3_KEY")?,
        secret: env("FLEET_S3_SECRET")?,
        public_endpoint: None,
        http: reqwest::Client::new(),
    });
    let root = Bucket {
        bucket: String::new(),
        ..(*bucket).clone()
    };
    match root.object(reqwest::Method::PUT, "apps", None).await? {
        (200 | 409, _) => {}
        (status, _) => return Err(format!("the bucket `apps`: the store answered {status}")),
    }
    let base = Arc::new(base);
    let gate = Arc::new(Semaphore::new(UPLOADS));
    let mut uploads = JoinSet::new();
    for n in 0..apps {
        let (bucket, base, gate) = (bucket.clone(), base.clone(), gate.clone());
        uploads.spawn(async move {
            let _permit = gate.acquire_owned().await.map_err(|err| err.to_string())?;
            let bytes = variant(&base, n);
            let hex = hex::encode(Sha256::digest(&bytes));
            let seed = format!("apps/{}/{}/seed.txt", n % shards, id_of(n));
            for (key, body) in [
                (component_key(&hex), bytes),
                (seed, format!("seed {n}").into_bytes()),
            ] {
                match bucket
                    .object(reqwest::Method::PUT, &key, Some((body, None)))
                    .await?
                {
                    (200, _) => {}
                    (status, _) => return Err(format!("{key}: the store answered {status}")),
                }
            }
            Ok::<_, String>((n, format!("sha256:{hex}")))
        });
    }
    let mut digests = BTreeMap::new();
    while let Some(done) = uploads.join_next().await {
        let (n, digest) = done.map_err(|err| err.to_string())??;
        digests.insert(n, digest);
    }
    eprintln!(
        "fleet: {apps} components and prefixes in {:?}",
        started.elapsed()
    );

    for shard in 0..shards {
        let placed: Vec<serde_json::Value> = digests
            .iter()
            .filter(|(n, _)| *n % shards == shard)
            .map(|(n, digest)| {
                serde_json::json!({"name": format!("load-{n}"), "id": id_of(*n), "tenant": format!("t{}", n % 100), "digest": digest})
            })
            .collect();
        let file = out.join(format!("shard-{shard}.json"));
        let text = serde_json::json!({"shard": shard.to_string(), "apps": placed}).to_string();
        std::fs::write(&file, text).map_err(|err| format!("{}: {err}", file.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_app_gets_its_own_digest_and_a_dev_shaped_id() {
        let base = b"\0asm\x0d\0\x01\0".to_vec();
        assert_ne!(
            Sha256::digest(variant(&base, 1)),
            Sha256::digest(variant(&base, 2))
        );
        assert_eq!(&variant(&base, 7)[..base.len()], &base[..]);
        // apps-db's pg_hba lets `app_<16 hex>_owner` log in, so the ids are shaped as dev's.
        assert_eq!(id_of(9_999), "000000000000270f");
    }

    #[test]
    fn an_app_is_migrated_as_its_owner_in_its_own_schema() {
        let sql = app_statements(3, 2, "postgres", "create table notes (id int);\n").unwrap();
        assert!(
            sql.contains("grant app_0000000000000003 to wasm_host_1"),
            "{sql}"
        );
        assert!(sql.contains("set role app_0000000000000003_owner;\nset search_path to app_0000000000000003;\ncreate table notes (id int);"), "{sql}");
        assert!(sql.ends_with("reset role;\n"), "{sql}");
    }
}
