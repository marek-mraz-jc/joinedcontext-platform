//! What the gateway believes about a token, and what it refuses (T-0228, PF-45, PF-46).
//!
//! The realm here is a throwaway P-256 key generated per test run and published as a JWKS
//! exactly the way Keycloak publishes one, so the verifier is exercised through the same
//! path it uses in production and no private key is ever written into the repository.

mod common;

use common::{in_seconds, Realm, ISSUER, KID};
use context_gateway::auth::accounts::{accounts_of, client_id, ServiceAccounts};
use context_gateway::auth::token::{bearer, Rejected, Verifier};
use serde_json::json;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";

fn audiences() -> Vec<String> {
    vec![SLUG.to_owned()]
}

#[test]
fn a_token_bound_to_this_endpoint_verifies() {
    let realm = Realm::new();
    let verifier = realm.verifier();
    let token = realm.workload_token("ovzdusie-etl-ovzdusie", json!(SLUG));

    let claims = verifier
        .verify(&token, &audiences())
        .expect("a valid token");
    assert_eq!(claims.iss, ISSUER);
    assert_eq!(claims.azp.as_deref(), Some("ovzdusie-etl-ovzdusie"));

    // An `aud` array containing the resource is the shape Keycloak actually emits.
    let many = realm.workload_token("ovzdusie-etl-ovzdusie", json!(["account", SLUG]));
    verifier
        .verify(&many, &audiences())
        .expect("one of the audiences names us");
}

/// RFC 8707: a token minted for one endpoint is useless at the next one, which is the
/// whole point of binding it.
#[test]
fn a_token_for_another_resource_is_refused() {
    let realm = Realm::new();
    let verifier = realm.verifier();

    for foreign in [
        json!("t9x2wqvn7mzc4hd6bkp3rjs5ga"),
        json!("account"),
        json!(["account", "portal"]),
    ] {
        let token = realm.workload_token("ovzdusie-etl-ovzdusie", foreign.clone());
        assert_eq!(
            verifier.verify(&token, &audiences()).err(),
            Some(Rejected::WrongAudience),
            "{foreign} was accepted"
        );
    }

    // No audience to check against is not a free pass.
    let token = realm.workload_token("ovzdusie-etl-ovzdusie", json!(SLUG));
    assert_eq!(
        verifier.verify(&token, &[]).err(),
        Some(Rejected::WrongAudience)
    );
}

#[test]
fn an_expired_token_is_refused() {
    let realm = Realm::new();
    let verifier = realm.verifier();
    let token = realm.mint(&json!({
        "iss": ISSUER,
        "sub": "service-account-etl",
        "aud": SLUG,
        "azp": "ovzdusie-etl-ovzdusie",
        "exp": in_seconds(-3600),
        "iat": in_seconds(-7200),
    }));

    assert_eq!(
        verifier.verify(&token, &audiences()).err(),
        Some(Rejected::Expired)
    );
}

#[test]
fn a_token_from_another_realm_is_refused() {
    let realm = Realm::new();
    let verifier = realm.verifier();
    let token = realm.mint(&json!({
        "iss": "https://evil.example/realms/joinedcontext",
        "sub": "service-account-etl",
        "aud": SLUG,
        "exp": in_seconds(300),
    }));

    assert_eq!(
        verifier.verify(&token, &audiences()).err(),
        Some(Rejected::WrongIssuer)
    );
}

/// The signature is checked against the realm's published key, so a token minted by
/// somebody else's key is refused even when every claim in it is right.
#[test]
fn a_token_signed_by_another_key_is_refused() {
    let realm = Realm::new();
    let impostor = Realm::new();
    let verifier = realm.verifier();
    let token = impostor.workload_token("ovzdusie-etl-ovzdusie", json!(SLUG));

    assert_eq!(
        verifier.verify(&token, &audiences()).err(),
        Some(Rejected::BadSignature)
    );
}

#[test]
fn a_token_naming_a_key_the_realm_never_published_is_refused() {
    let realm = Realm::new();
    let verifier = realm.verifier();
    let token = realm.mint_with_kid(
        "some-other-key",
        &json!({ "iss": ISSUER, "sub": "x", "aud": SLUG, "exp": in_seconds(300) }),
    );

    assert_eq!(
        verifier.verify(&token, &audiences()).err(),
        Some(Rejected::UnknownKey)
    );

    // A verifier with no keys at all refuses everything, which is what it does between
    // start-up and the first successful JWKS fetch.
    let empty = Verifier::new(ISSUER);
    let good = realm.workload_token("x", json!(SLUG));
    assert_eq!(
        empty.verify(&good, &audiences()).err(),
        Some(Rejected::UnknownKey)
    );
}

/// The algorithm comes from the published key, never from the header a caller wrote.
#[test]
fn a_token_without_a_key_id_or_with_a_forged_algorithm_is_refused() {
    let realm = Realm::new();
    let verifier = realm.verifier();

    let no_kid = realm.mint_without_kid(
        &json!({ "iss": ISSUER, "sub": "x", "aud": SLUG, "exp": in_seconds(300) }),
    );
    assert_eq!(
        verifier.verify(&no_kid, &audiences()).err(),
        Some(Rejected::Malformed)
    );

    // `alg: none`, the oldest trick there is.
    let unsigned = format!(
        "{}.{}.",
        common::b64(&json!({ "alg": "none", "kid": KID }).to_string()),
        common::b64(
            &json!({ "iss": ISSUER, "sub": "x", "aud": SLUG, "exp": in_seconds(300) }).to_string()
        ),
    );
    assert!(verifier.verify(&unsigned, &audiences()).is_err());

    for garbage in ["", "not.a.token", "a.b", "....."] {
        assert!(
            verifier.verify(garbage, &audiences()).is_err(),
            "{garbage:?}"
        );
    }
}

/// The `Authorization` header carries two different credentials; a platform API key is not
/// a JWT and must not be handed to the JWT verifier (PF-36, PF-37).
#[test]
fn the_bearer_header_separates_a_token_from_an_api_key() {
    assert_eq!(
        bearer(Some("Bearer eyJhbGciOiJFUzI1NiJ9.x.y")),
        Ok("eyJhbGciOiJFUzI1NiJ9.x.y")
    );
    for not_a_token in [
        None,
        Some("Bearer "),
        Some("Basic dXNlcjpwYXNz"),
        Some("Bearer jc_k1_secret"),
        Some("eyJhbGciOiJFUzI1NiJ9.x.y"),
    ] {
        assert_eq!(
            bearer(not_a_token),
            Err(Rejected::NoToken),
            "{not_a_token:?}"
        );
    }
}

/// PF-46: `azp` resolves through the manifests, and the client id is derived from them,
/// so it is the same string on both sides without anybody writing it down twice.
#[test]
fn azp_resolves_to_the_service_account_the_repository_declares() {
    let dir = std::env::temp_dir().join("gateway-accounts-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("projects/ovzdusie/access/serviceaccounts"))
        .expect("a repository");
    std::fs::write(
        dir.join("projects/ovzdusie/access/serviceaccounts/etl.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ServiceAccount
metadata:
  name: etl
  namespace: ovzdusie
spec:
  owner:
    user: demo.steward
  purpose: "MQTT ingest of air-quality readings"
  roles:
    - role: space-writer
      scope:
        contextSpace: ovzdusie
    - role: doprava-reader
      scope:
        contextSpace: doprava
  credentials:
    - kind: oauth-client
      name: default
"#,
    )
    .expect("the manifest is written");
    // The space pins its segment; `doprava` is not in the repository and renders (PF-84).
    std::fs::create_dir_all(dir.join("projects/ovzdusie/spaces/ovzdusie")).expect("a space");
    std::fs::write(
        dir.join("projects/ovzdusie/spaces/ovzdusie/space.yaml"),
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: ovzdusie\n  namespace: ovzdusie\nspec:\n  urnSegment: ovzdusie\n",
    )
    .expect("the space is written");

    let repo = jcctl::loader::Repository::load(&dir).expect("the repository loads");
    let accounts = accounts_of(&repo);
    assert_eq!(accounts.len(), 1);

    assert_eq!(client_id("ovzdusie", "etl"), "ovzdusie-etl");
    let account = accounts.resolve("ovzdusie-etl").expect("azp resolves");
    assert_eq!(account.name, "etl");
    assert_eq!(account.project, "ovzdusie");

    // A role scoped to another space is not a role here (PF-35).
    let here = account.roles_in("ovzdusie", "ovzdusie");
    assert!(here.contains("space-writer"));
    assert!(!here.contains("doprava-reader"));

    assert!(
        accounts.resolve("etl").is_none(),
        "the bare name is not the client id"
    );
    assert!(accounts.resolve("ovzdusie-somebody-else").is_none());
    assert!(ServiceAccounts::new().resolve("ovzdusie-etl").is_none());

    std::fs::remove_dir_all(&dir).expect("clean up");
}

/// T-1454: the hyphen that joins project and name is a character of both, so `helsinki` +
/// `kpi-writer` and `helsinki-kpi` + `writer` derive one client id. That id resolves to nobody,
/// whichever manifest is read first; an account whose id is its own keeps resolving.
#[test]
fn a_client_id_two_accounts_derive_resolves_to_nobody() {
    let dir = std::env::temp_dir().join("gateway-accounts-ambiguous-test");
    let _ = std::fs::remove_dir_all(&dir);
    for (project, name) in [
        ("helsinki", "kpi-writer"),
        ("helsinki-kpi", "writer"),
        ("helsinki", "etl"),
    ] {
        let folder = dir.join(format!("projects/{project}/access/serviceaccounts"));
        std::fs::create_dir_all(&folder).expect("a repository");
        std::fs::write(
            folder.join(format!("{name}.yaml")),
            format!(
                r#"apiVersion: joinedcontext.com/v1alpha1
kind: ServiceAccount
metadata:
  name: {name}
  namespace: {project}
spec:
  owner:
    user: demo.steward
  purpose: "writes {project}"
  roles:
    - role: space-writer
      scope:
        project: {project}
  credentials:
    - kind: oauth-client
      name: default
"#
            ),
        )
        .expect("the manifest is written");
    }

    let repo = jcctl::loader::Repository::load(&dir).expect("the repository loads");
    let accounts = accounts_of(&repo);
    assert!(
        accounts.resolve("helsinki-kpi-writer").is_none(),
        "an ambiguous client id is nobody's"
    );
    assert_eq!(
        accounts.resolve("helsinki-etl").map(|a| a.name.as_str()),
        Some("etl")
    );
    assert_eq!(accounts.len(), 1);

    std::fs::remove_dir_all(&dir).expect("clean up");
}
