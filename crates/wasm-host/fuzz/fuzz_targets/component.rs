//! Arbitrary bytes as an App's component, through the shard's own path: fetched by digest,
//! compiled by the production engine and linker, and when it compiles, served one request under a
//! short wall time. Any outcome but a panic, an abort or a hang of the host is fine. Each run waits
//! until its instance has ended, so none runs on into the next run's leak check (T-4807).

#![no_main]

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use sha2::{Digest, Sha256};
use wasm_host::host::Host;
use wasm_host::limits::Limits;
use wasm_host::placement::Placed;
use wasm_host::source::Source;
use wasm_host::storage::Unconfigured;

struct World {
    runtime: tokio::runtime::Runtime,
    dir: std::path::PathBuf,
    host: Arc<Host>,
}

fn world() -> &'static World {
    static WORLD: OnceLock<World> = OnceLock::new();
    WORLD.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let dir = std::env::temp_dir().join(format!("wasm-host-fuzz-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let limits = Limits {
            wall_time: Duration::from_millis(100),
            memory_bytes: 16 << 20,
            cached_components: 1,
            pooled_instances: 16,
            per_app_concurrency: 1,
            ..Limits::default()
        };
        let host = runtime
            .block_on(async {
                Host::new(
                    limits,
                    Source::Dir(dir.clone()),
                    Arc::new(Unconfigured),
                    None,
                    None,
                )
            })
            .expect("host");
        World { runtime, dir, host }
    })
}

fuzz_target!(|bytes: &[u8]| {
    let world = world();
    let hex = hex::encode(Sha256::digest(bytes));
    let file = world.dir.join(format!("sha256-{hex}.wasm"));
    std::fs::write(&file, bytes).expect("component");
    let app = Placed {
        name: "fuzz".into(),
        id: "fuzz-id".into(),
        tenant: "fuzz".into(),
        digest: format!("sha256:{hex}"),
        endpoint: None,
        jobs: Vec::new(),
    };
    world.runtime.block_on(async {
        if world.host.component(&app.digest).await.is_ok() {
            let request = http::Request::builder()
                .uri("http://apps.test/apps/fuzz/api/hello")
                .body(Bytes::new())
                .expect("request");
            let _ = world.host.serve(&app, request, None).await;
            // The App's one place comes back once its instance ends; libFuzzer's -timeout reports
            // one that never does.
            while world.host.admit(&app).is_err() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    });
    let _ = std::fs::remove_file(&file);
});
