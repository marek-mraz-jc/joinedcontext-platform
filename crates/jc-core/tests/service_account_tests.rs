use jc_core::kinds::{CredentialKind, RoleScope, ServiceAccount};

const GOLDEN: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ServiceAccount
metadata:
  name: vendorx-parking-push
  namespace: bb-doprava
  title: { sk: "VendorX parkovanie – priamy zápis", en: "VendorX parking – direct write" }
spec:
  owner: { user: "jana.k@bb" }                    # accountable human, notified on expiry/anomaly
  purpose: "VendorX cloud pushes ParkingSpot updates every 10 s"
  roles:                                          # same role templates as humans (§2), scoped
    - { role: space-writer, scope: { contextSpace: parking }, types: [ParkingSpot], operations: [createEntity, updateAttrs] }   # CIM 009 clause 4.20 names only (R8)
  credentials:
    - kind: oauth-client         # Keycloak confidential client, client_credentials → short-lived JWT (preferred)
      name: main
  limits: { requestsPerMinute: 1200 }
# status (never in Git): keycloak clientId, lastUsedAt per credential, rotation state
"#;

#[test]
fn test_service_account_golden_manifest_roundtrip() {
    let sa = ServiceAccount::from_yaml(GOLDEN).expect("parses golden manifest");
    sa.validate().expect("golden manifest is valid");

    assert_eq!(sa.metadata.name, "vendorx-parking-push");
    assert_eq!(sa.metadata.namespace.as_deref(), Some("bb-doprava"));
    assert_eq!(sa.spec.owner.user, "jana.k@bb");
    assert_eq!(
        sa.spec.purpose,
        "VendorX cloud pushes ParkingSpot updates every 10 s"
    );
    assert_eq!(sa.spec.roles.len(), 1);
    assert_eq!(sa.spec.roles[0].role, "space-writer");
    assert_eq!(
        sa.spec.roles[0].scope.context_space.as_deref(),
        Some("parking")
    );
    assert_eq!(sa.spec.roles[0].types, vec!["ParkingSpot"]);
    assert_eq!(sa.spec.roles[0].operations.len(), 2);

    assert_eq!(sa.spec.credentials.len(), 1);
    assert_eq!(sa.spec.credentials[0].kind, CredentialKind::OauthClient);
    assert_eq!(sa.spec.credentials[0].name, "main");

    assert_eq!(
        sa.spec.limits.as_ref().map(|l| l.requests_per_minute),
        Some(1200)
    );

    let yaml = sa.to_yaml().expect("serializes to yaml");
    let roundtripped = ServiceAccount::from_yaml(&yaml).expect("deserializes back");
    assert_eq!(sa, roundtripped);

    let path = sa.resource_path().expect("resource path derives");
    assert_eq!(
        path,
        "projects/bb-doprava/access/serviceaccounts/vendorx-parking-push.yaml"
    );
}

#[test]
fn test_pf36_credential_rejects_secret_fields() {
    let forbidden = ["secret", "value", "password", "clientSecret", "apiKey"];
    for field in forbidden {
        let yaml = format!(
            r#"
apiVersion: joinedcontext.com/v1alpha1
kind: ServiceAccount
metadata:
  name: test-sa
  namespace: bb-doprava
spec:
  owner: {{ user: "jana.k@bb" }}
  purpose: "test"
  roles:
    - role: test-role
      scope: {{ project: bb-doprava }}
  credentials:
    - kind: oauth-client
      name: main
      {field}: "forbidden-secret-token"
"#
        );
        let res = ServiceAccount::from_yaml(&yaml);
        assert!(
            res.is_err(),
            "expected parse error for forbidden credential field `{field}`, got: {res:?}"
        );
    }
}

#[test]
fn test_role_scope_single_target_invariant() {
    let base_sa = ServiceAccount::from_yaml(GOLDEN).expect("golden");

    // Zero scope targets
    let mut no_scope_sa = base_sa.clone();
    no_scope_sa.spec.roles[0].scope = RoleScope::default();
    assert!(no_scope_sa.validate().is_err());

    // Two scope targets
    let mut multi_scope_sa = base_sa.clone();
    multi_scope_sa.spec.roles[0].scope = RoleScope {
        context_space: Some("parking".to_string()),
        project: Some("bb-doprava".to_string()),
        organization: None,
    };
    assert!(multi_scope_sa.validate().is_err());
}

#[test]
fn test_ip_allow_list_cidr_validation() {
    let sa = ServiceAccount::from_yaml(GOLDEN).expect("golden");

    let invalid_cidrs = [
        "185.12.0.0",
        "185.12.0.0/33",
        "185.12.0.0/x",
        "not-an-ip/22",
        "2001:db8::/129",
    ];

    for cidr in invalid_cidrs {
        let mut test_sa = sa.clone();
        test_sa.spec.credentials[0].ip_allow_list = vec![cidr.to_string()];
        assert!(
            test_sa.validate().is_err(),
            "expected CIDR validation failure for `{cidr}`"
        );
    }

    let valid_cidrs = ["185.12.0.0/22", "10.0.0.1/32", "2001:db8::/32"];
    for cidr in valid_cidrs {
        let mut test_sa = sa.clone();
        test_sa.spec.credentials[0].ip_allow_list = vec![cidr.to_string()];
        assert!(
            test_sa.validate().is_ok(),
            "expected CIDR validation success for `{cidr}`"
        );
    }
}

#[test]
fn test_role_rejects_proprietary_operation_verb() {
    let bad_yaml = GOLDEN.replace("[createEntity, updateAttrs]", "[upsertEntity]");
    let res = ServiceAccount::from_yaml(&bad_yaml);
    assert!(
        res.is_err(),
        "expected deserialization error for non-CIM 009 operation `upsertEntity`"
    );
}

/// PF-37 (owner decision 2026-10-18, T-4362): the platform mints and accepts no API key, so a
/// manifest declaring one does not read, rather than declaring a credential nothing honours.
#[test]
fn an_api_key_credential_is_no_credential_kind() {
    let yaml = GOLDEN.replace(
        "      name: main\n",
        "      name: main\n    - kind: api-key\n      name: legacy-push\n",
    );
    assert!(yaml.contains("kind: api-key"));
    let refused = ServiceAccount::from_yaml(&yaml).expect_err("api-key is refused");
    assert!(refused.to_string().contains("api-key"), "{refused}");
}

#[test]
fn test_empty_roles_and_credentials_fail_validation() {
    let mut sa1 = ServiceAccount::from_yaml(GOLDEN).expect("golden");
    sa1.spec.roles.clear();
    assert!(sa1.validate().is_err());

    let mut sa2 = ServiceAccount::from_yaml(GOLDEN).expect("golden");
    sa2.spec.credentials.clear();
    assert!(sa2.validate().is_err());
}

/// T-1454: the Keycloak client id is `{project}-{name}`, and the realm's own workloads have
/// clients of that shape too. An account that would derive one is refused, naming the id; the
/// same name in another project is an ordinary account.
#[test]
fn an_account_that_derives_a_platform_client_id_is_refused() {
    use jc_core::kinds::service_account::{keycloak_client_id, PLATFORM_CLIENTS};
    let account = |project: &str, name: &str| {
        GOLDEN
            .replace("name: vendorx-parking-push", &format!("name: {name}"))
            .replace("namespace: bb-doprava", &format!("namespace: {project}"))
    };
    for (project, name) in [
        ("activity", "ingest"),
        ("portal", "api"),
        ("apisix", "gateway"),
    ] {
        let sa = ServiceAccount::from_yaml(&account(project, name)).expect("parses");
        let err = sa
            .validate()
            .expect_err("a platform client id is refused")
            .to_string();
        let id = keycloak_client_id(project, name);
        assert!(PLATFORM_CLIENTS.contains(&id.as_str()));
        assert!(err.contains(&id), "the refusal names the id: {err}");
    }
    for (project, name) in [
        ("helsinki", "ingest"),
        ("activity", "ingest-2"),
        ("portal-x", "api"),
    ] {
        let sa = ServiceAccount::from_yaml(&account(project, name)).expect("parses");
        sa.validate()
            .unwrap_or_else(|err| panic!("{project}/{name} is an ordinary account: {err}"));
    }
}

/// AP-159 (T-3372): `appjob-` names the account a wasm App's jobs run as; one written by hand
/// could take an App's client and its Endpoint, so it is refused, naming why.
#[test]
fn an_account_written_by_hand_cannot_take_an_apps_job_principal() {
    let named = |name: &str| GOLDEN.replace("name: vendorx-parking-push", &format!("name: {name}"));
    let err = ServiceAccount::from_yaml(&named("appjob-kpi-forecast"))
        .expect("parses")
        .validate()
        .expect_err("a job principal's name is refused")
        .to_string();
    assert!(
        err.contains("AP-159") && err.contains("appjob-kpi-forecast"),
        "{err}"
    );
    for name in ["app-builder", "appjobs", "kpi-appjob"] {
        ServiceAccount::from_yaml(&named(name))
            .expect("parses")
            .validate()
            .unwrap_or_else(|err| panic!("{name} is an ordinary account: {err}"));
    }
}

/// PL-19 (T-1508): `pl-` names the account every Pipeline is given; an account written by hand
/// that takes the prefix could take a pipeline's client and grants, so it is refused, naming why.
#[test]
fn an_account_written_by_hand_cannot_take_a_pipelines_name() {
    let named = |name: &str| GOLDEN.replace("name: vendorx-parking-push", &format!("name: {name}"));
    let err = ServiceAccount::from_yaml(&named("pl-drepo"))
        .expect("parses")
        .validate()
        .expect_err("a pipeline's name is refused")
        .to_string();
    assert!(err.contains("PL-19") && err.contains("pl-drepo"), "{err}");
    for name in ["pipelines", "plan-push", "drepo-pl"] {
        ServiceAccount::from_yaml(&named(name))
            .expect("parses")
            .validate()
            .unwrap_or_else(|err| panic!("{name} is an ordinary account: {err}"));
    }
}

/// T-4015, PF-47: a device signs in by its own key pair. The manifest holds the public half only.
fn device(credential: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1
kind: ServiceAccount
metadata:
  name: cam-north
  namespace: helsinki
spec:
  owner: {{ user: \"jana.k@bb\" }}
  purpose: \"Camera north publishes frames to the cameras broker\"
  roles:
    - {{ role: space-writer, scope: {{ contextSpace: cameras }} }}
  credentials:
{credential}"
    )
}

const EC_X: &str = "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU";
const EC_Y: &str = "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0";

fn ec_key(kid: &str) -> String {
    format!("{{ kty: EC, crv: P-256, kid: {kid}, x: {EC_X}, y: {EC_Y}, alg: ES256, use: sig }}")
}

fn oauth_with(keys: &[String]) -> String {
    format!(
        "    - kind: oauth-client\n      name: main\n      jwks:\n        keys: [{}]\n",
        keys.join(", ")
    )
}

fn refusal(yaml: &str) -> String {
    match ServiceAccount::from_yaml(yaml) {
        Ok(account) => account.validate().expect_err("refused").to_string(),
        Err(error) => error.to_string(),
    }
}

#[test]
fn a_device_account_with_a_public_ec_key_reads_and_names_it() {
    let account =
        ServiceAccount::from_yaml(&device(&oauth_with(&[ec_key("cam-2026")]))).expect("parses");
    account.validate().expect("a public P-256 key is valid");
    let jwks = |spec: &jc_core::kinds::service_account::ServiceAccountSpec| {
        spec.credentials.iter().find_map(|c| c.jwks.clone())
    };
    let keys = jwks(&account.spec).expect("the device's keys");
    assert_eq!(keys.keys[0].kid, "cam-2026");
    let again = ServiceAccount::from_yaml(&account.to_yaml().expect("yaml")).expect("reads back");
    assert_eq!(jwks(&again.spec), Some(keys));
}

#[test]
fn a_private_or_symmetric_key_never_reads() {
    let private = format!(
        "{{ kty: EC, crv: P-256, kid: k, x: {EC_X}, y: {EC_Y}, d: jpsQnnGQmL-YBIffH1136cspYG6-0iY7X1fCE9-E9LI }}"
    );
    assert!(refusal(&device(&oauth_with(&[private]))).contains("unknown field `d`"));
    let symmetric = "{ kty: oct, kid: k, k: c2VjcmV0 }".to_owned();
    assert!(refusal(&device(&oauth_with(&[symmetric]))).contains("unknown field `k`"));
    let oct_shape = "{ kty: oct, kid: k }".to_owned();
    assert!(refusal(&device(&oauth_with(&[oct_shape]))).contains("EC (P-256) or RSA"));
}

#[test]
fn a_device_key_set_is_one_to_five_keys_each_named_once_and_well_formed() {
    assert!(refusal(&device(&oauth_with(&[]))).contains("one to five"));
    let six: Vec<String> = (0..6).map(|i| ec_key(&format!("k{i}"))).collect();
    assert!(refusal(&device(&oauth_with(&six))).contains("one to five"));
    assert!(refusal(&device(&oauth_with(&[ec_key("k"), ec_key("k")]))).contains("share this kid"));
    for (key, why) in [
        (
            format!("{{ kty: EC, crv: P-384, kid: k, x: {EC_X}, y: {EC_Y} }}"),
            "P-256",
        ),
        (
            format!("{{ kty: EC, crv: P-256, kid: k, x: short, y: {EC_Y} }}"),
            "32 bytes",
        ),
        (
            format!("{{ kty: EC, crv: P-256, kid: k, x: \"{EC_X}=\", y: {EC_Y} }}"),
            "32 bytes",
        ),
        (
            format!("{{ kty: EC, crv: P-256, kid: k, x: {EC_X}, y: {EC_Y}, use: enc }}"),
            "signing",
        ),
        (
            format!("{{ kty: EC, crv: P-256, kid: k, x: {EC_X}, y: {EC_Y}, n: AQAB }}"),
            "no n or e",
        ),
        (
            format!("{{ kty: EC, crv: P-256, kid: \"\", x: {EC_X}, y: {EC_Y} }}"),
            "kid",
        ),
        (
            "{ kty: RSA, kid: k, n: AQAB, e: AQAB }".to_owned(),
            "2048 bits",
        ),
    ] {
        let message = refusal(&device(&oauth_with(std::slice::from_ref(&key))));
        assert!(message.contains(why), "{key}: {message}");
    }
    let rsa = format!(
        "{{ kty: RSA, kid: k, n: {}, e: AQAB, alg: RS256 }}",
        "u".repeat(342)
    );
    ServiceAccount::from_yaml(&device(&oauth_with(&[rsa])))
        .expect("parses")
        .validate()
        .expect("an RSA-2048 key is valid");
}

#[test]
fn keys_belong_on_an_account_bound_to_no_workload() {
    let bound = format!(
        "{}  workload:\n    kubernetes: {{ namespace: cams, serviceAccount: north }}\n",
        device(&oauth_with(&[ec_key("k")]))
    );
    assert!(refusal(&bound).contains("pod's token"));
}
