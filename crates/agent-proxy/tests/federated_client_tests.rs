//! The proxy proves its own Keycloak client with the pod's projected ServiceAccount token and
//! no secret (PF-47, T-2868).
//!
//! Keycloak finds a `federated-jwt` client by the token's subject and refuses a request that also
//! names `client_id`, so every call the proxy makes as its client carries the assertion alone.

use std::sync::Arc;

use agent_proxy::config::{Config, ConfigError};
use agent_proxy::delegation::Grants;
use agent_proxy::inject::CredentialManager;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ASSERTION_TYPE: &str =
    "client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer";

fn token_file(name: &str, content: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("jc-proxy-federated-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch dir");
    let file = dir.join("token");
    std::fs::write(&file, content).expect("the token file");
    file
}

fn config(issuer: &str, vars: &[(&str, String)]) -> Result<Config, ConfigError> {
    let issuer = issuer.to_owned();
    let vars: Vec<(String, String)> = vars
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect();
    Config::from_lookup(move |key| {
        if key == "JC_OIDC_ISSUER" {
            return Some(issuer.clone());
        }
        vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    })
}

async fn realm() -> MockServer {
    let realm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/realms/jc/protocol/openid-connect/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "access_token": "proxy-own-token", "expires_in": 300 })),
        )
        .mount(&realm)
        .await;
    Mock::given(method("POST"))
        .and(path("/realms/jc/protocol/openid-connect/token/introspect"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "active": false })))
        .mount(&realm)
        .await;
    realm
}

async fn bodies(realm: &MockServer) -> Vec<String> {
    realm
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect()
}

#[tokio::test]
async fn the_proxys_grants_carry_the_projected_token_and_no_client_id_or_secret() {
    let realm = realm().await;
    let file = token_file("grants", "eyJ.projected\n");
    let config = Arc::new(
        config(
            &format!("{}/realms/jc", realm.uri()),
            &[("JC_OIDC_CLIENT_ASSERTION_FILE", file.display().to_string())],
        )
        .expect("a federated configuration"),
    );

    let token = CredentialManager::new(Arc::clone(&config))
        .get_endpoint_token("helsinki-air")
        .await
        .expect("an endpoint token");
    assert_eq!(token, "proxy-own-token");
    let seen = Grants::new(Arc::clone(&config))
        .is_portal("some-bearer")
        .await
        .expect("an introspection");
    assert!(!seen);

    let bodies = bodies(&realm).await;
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    for body in &bodies {
        assert!(body.contains(ASSERTION_TYPE), "{body}");
        assert!(body.contains("client_assertion=eyJ.projected"), "{body}");
        assert!(!body.contains("client_id="), "{body}");
        assert!(!body.contains("client_secret="), "{body}");
    }
}

#[tokio::test]
async fn an_empty_token_file_asks_the_realm_nothing() {
    let realm = realm().await;
    let file = token_file("empty", "\n");
    let config = Arc::new(
        config(
            &format!("{}/realms/jc", realm.uri()),
            &[("JC_OIDC_CLIENT_ASSERTION_FILE", file.display().to_string())],
        )
        .expect("a federated configuration"),
    );

    CredentialManager::new(config)
        .get_endpoint_token("helsinki-air")
        .await
        .expect_err("no grant without an assertion");
    assert!(bodies(&realm).await.is_empty());
}

#[test]
fn a_secret_beside_the_projected_token_is_refused_at_startup() {
    let error = config(
        "http://keycloak/realms/jc",
        &[
            (
                "JC_OIDC_CLIENT_ASSERTION_FILE",
                "/var/run/secrets/jc/keycloak/token".to_owned(),
            ),
            ("JC_OIDC_CLIENT_SECRET", "a-secret".to_owned()),
        ],
    )
    .expect_err("a federated client holds no secret beside it");
    assert!(!error.to_string().contains("a-secret"), "{error}");
}

#[test]
fn the_projected_token_stands_for_the_client_secret_at_startup() {
    let federated = config(
        "http://keycloak/realms/jc",
        &[
            (
                "JC_OIDC_CLIENT_ASSERTION_FILE",
                "/var/run/secrets/jc/keycloak/token".to_owned(),
            ),
            ("JC_FORGE_TOKEN", "forge".to_owned()),
            ("JC_MODEL_KEY", "model".to_owned()),
        ],
    )
    .expect("a federated configuration");
    federated
        .require_secrets()
        .expect("no client secret is needed");

    let neither = config(
        "http://keycloak/realms/jc",
        &[
            ("JC_FORGE_TOKEN", "forge".to_owned()),
            ("JC_MODEL_KEY", "model".to_owned()),
        ],
    )
    .expect("a configuration");
    assert!(neither.require_secrets().is_err());
}
