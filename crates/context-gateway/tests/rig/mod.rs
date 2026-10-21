//! What both WebSocket rigs share (`ws_fanout_rig`, `ws_abuse_rig`): the repository of one public
//! Vehicle Endpoint, a stub broker whose view socket forwards every line the rig pushes, and the
//! numbers a run reports.

use axum::extract::ws::{Message as AxumMessage, WebSocketUpgrade};
use axum::routing::get;
use axum::Router;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub const SLUG: &str = "vvvvvvvvvvvvvvvvvvvvvvvvvv";

pub fn repo() -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-ws-rig-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    let write = |name: &str, body: &str| std::fs::write(dir.join(name), body).expect("write");
    write(
        "space.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: transport\n  \
         namespace: helsinki\nspec:\n  isSandbox: false\n  urnSegment: transport\n",
    );
    write(
        "endpoint.yaml",
        &format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: vehicles\n  \
             namespace: helsinki\nspec:\n  contextSpaceRef: transport\n  slug: {SLUG}\n  audience: public\n"
        ),
    );
    write(
        "policy.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Policy\nmetadata:\n  name: public-vehicles\n  \
         namespace: helsinki\nspec:\n  contextSpaceRef: { kind: ContextSpace, name: transport }\n  \
         assigner: did:web:hel.fi\n  assignee: { kind: role, id: public }\n  \
         operations: [retrieveOps]\n  information:\n    - entities:\n        - type: Vehicle\n",
    );
    dir
}

/// A broker whose view socket welcomes, accepts the gateway's subscription and forwards every
/// line the rig pushes; its entity query answers an empty page.
pub async fn broker() -> (String, tokio::sync::broadcast::Sender<Arc<String>>) {
    let (push, _) = tokio::sync::broadcast::channel::<Arc<String>>(1 << 16);
    let held = push.clone();
    let app = Router::new()
        .route(
            "/x/views/{view}/ws",
            get(move |upgrade: WebSocketUpgrade| {
                let mut pushed = held.subscribe();
                async move {
                    upgrade.on_upgrade(|mut socket| async move {
                        let welcome = json!({"metadata": {"x-op": "x-welcome",
                            "x-connection": "c", "x-resume-token": "t"}});
                        let _ = socket.send(AxumMessage::Text(welcome.to_string().into())).await;
                        loop {
                            tokio::select! {
                                read = socket.recv() => {
                                    let Some(Ok(AxumMessage::Text(text))) = read else { return };
                                    if text.as_str().contains("x-subscribe") {
                                        let created = json!({"metadata": {"x-op": "x-result",
                                            "x-id": "up", "x-status": 201}});
                                        let _ = socket.send(AxumMessage::Text(created.to_string().into())).await;
                                    }
                                }
                                Ok(text) = pushed.recv() => {
                                    if socket.send(AxumMessage::Text(text.as_str().into())).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    })
                }
            }),
        )
        .route(
            "/x/views/{view}/ngsi-ld/v1/entities",
            get(|| async { axum::Json(json!([])) }),
        );
    (serve(app).await, push)
}

pub async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("127.0.0.1:{}", address.port())
}

pub fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let at = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[at]
}

/// `VmRSS` and user + system CPU of this process, from `/proc` (Linux runners).
pub fn usage() -> (u64, Duration) {
    let rss = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .unwrap_or(0);
    let cpu = std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            let after = s.rsplit_once(')')?.1;
            let fields: Vec<&str> = after.split_whitespace().collect();
            let ticks: u64 =
                fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
            // ponytail: the usual 100 ticks per second; sysconf would be a libc dependency.
            Some(Duration::from_millis(ticks * 10))
        })
        .unwrap_or_default();
    (rss, cpu)
}
