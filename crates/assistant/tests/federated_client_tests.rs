//! The assistant proves its Keycloak client with the pod's projected ServiceAccount token and
//! no secret (PF-47, T-2868): the service token and the introspection of a caller's token both
//! carry the assertion and name no client.

use assistant::chat::model::{Model, ModelConfig};
use jc_core::ClientCredential;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn the_service_token_and_introspection_carry_the_projected_token_alone() {
    let realm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "access_token": "assistant-token", "expires_in": 300 })),
        )
        .mount(&realm)
        .await;
    Mock::given(method("POST"))
        .and(path("/token/introspect"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "active": false })))
        .mount(&realm)
        .await;
    let dir = std::env::temp_dir().join(format!("jc-assistant-federated-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch dir");
    let file = dir.join("token");
    std::fs::write(&file, "eyJ.projected\n").expect("the token file");

    let model = Model::new(
        ModelConfig {
            proxy: "http://proxy".into(),
            token_url: format!("{}/token", realm.uri()),
            client_id: "jc-assistant".into(),
            credential: ClientCredential::Assertion(file),
            model: "test/model".into(),
        },
        reqwest::Client::new(),
    );
    assert_eq!(model.token().await.expect("a token"), "assistant-token");
    assert!(!model
        .is_service_account("a-bearer", "portal-api")
        .await
        .expect("an answer"));

    let bodies: Vec<String> = realm
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect();
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    for body in &bodies {
        assert!(body.contains("client_assertion=eyJ.projected"), "{body}");
        assert!(!body.contains("client_id="), "{body}");
        assert!(!body.contains("client_secret="), "{body}");
    }
}
