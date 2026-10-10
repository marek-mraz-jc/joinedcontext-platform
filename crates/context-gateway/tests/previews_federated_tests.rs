//! The gateway proves its client with the pod's projected ServiceAccount token and no secret
//! (PF-47, T-2868).
//!
//! Keycloak's `federated-jwt` client finds the client by the token's subject, and refuses a
//! request that also names `client_id`, so the grant carries the assertion and nothing else.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::Form;
use axum::routing::post;
use axum::Router;
use serde_json::json;

use context_gateway::config::Credential;
use context_gateway::previews::WorkloadToken;

type Seen = Arc<std::sync::Mutex<Vec<HashMap<String, String>>>>;

async fn realm() -> (String, Seen) {
    let seen: Seen = Arc::default();
    let grants = Arc::clone(&seen);
    let app = Router::new().route(
        "/token",
        post(move |Form(grant): Form<HashMap<String, String>>| {
            let grants = Arc::clone(&grants);
            async move {
                if let Ok(mut seen) = grants.lock() {
                    seen.push(grant);
                }
                axum::Json(json!({ "access_token": "the-gateways-token", "expires_in": 0 }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/token"), seen)
}

fn token_file(name: &str, content: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("jc-gw-federated-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch dir");
    let path = dir.join("token");
    std::fs::write(&path, content).expect("the token file");
    path
}

#[tokio::test]
async fn the_grant_carries_the_projected_token_and_no_client_id_or_secret() {
    let (token_url, seen) = realm().await;
    let path = token_file("grant", "eyJ.first\n");
    let token = WorkloadToken::with_credential(
        token_url,
        "context-gateway".to_owned(),
        Credential::Assertion(path.clone()),
    );

    assert_eq!(token.get().await.expect("a token"), "the-gateways-token");
    // The kubelet rewrites the file before the token expires: each grant reads it again.
    std::fs::write(&path, "eyJ.second").expect("the rotated token");
    token.get().await.expect("a token");

    let grants = seen.lock().expect("the grants");
    assert_eq!(grants.len(), 2);
    for (grant, assertion) in grants.iter().zip(["eyJ.first", "eyJ.second"]) {
        assert_eq!(grant["grant_type"], "client_credentials");
        assert_eq!(
            grant["client_assertion_type"],
            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"
        );
        assert_eq!(grant["client_assertion"], assertion);
        assert!(!grant.contains_key("client_id"), "{grant:?}");
        assert!(!grant.contains_key("client_secret"), "{grant:?}");
    }
}

#[tokio::test]
async fn a_missing_or_empty_token_file_asks_the_realm_nothing() {
    let (token_url, seen) = realm().await;
    let empty = token_file("empty", " \n");
    for path in [empty.clone(), empty.with_file_name("absent")] {
        let token = WorkloadToken::with_credential(
            token_url.clone(),
            "context-gateway".to_owned(),
            Credential::Assertion(path),
        );
        token
            .get()
            .await
            .expect_err("no token without an assertion");
    }
    assert!(seen.lock().expect("the grants").is_empty());
}
