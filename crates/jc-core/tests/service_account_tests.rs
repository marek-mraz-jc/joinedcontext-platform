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
    - kind: api-key              # for systems that cannot do OAuth (devices, legacy ETL); hashed at rest, shown once
      name: legacy-push
      expiresAt: "2027-03-01T00:00:00Z"
      ipAllowList: ["185.12.0.0/22"]
  limits: { requestsPerMinute: 1200 }
# status (never in Git): keycloak clientId, keyIds, lastUsedAt per credential, rotation state
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

    assert_eq!(sa.spec.credentials.len(), 2);
    assert_eq!(sa.spec.credentials[0].kind, CredentialKind::OauthClient);
    assert_eq!(sa.spec.credentials[0].name, "main");
    assert_eq!(sa.spec.credentials[1].kind, CredentialKind::ApiKey);
    assert_eq!(sa.spec.credentials[1].name, "legacy-push");
    assert_eq!(sa.spec.credentials[1].ip_allow_list, vec!["185.12.0.0/22"]);

    assert_eq!(
        sa.spec.limits.as_ref().map(|l| l.requests_per_minute),
        Some(1200)
    );
    assert!(sa.spec.has_api_key());

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
        test_sa.spec.credentials[1].ip_allow_list = vec![cidr.to_string()];
        assert!(
            test_sa.validate().is_err(),
            "expected CIDR validation failure for `{cidr}`"
        );
    }

    let valid_cidrs = ["185.12.0.0/22", "10.0.0.1/32", "2001:db8::/32"];
    for cidr in valid_cidrs {
        let mut test_sa = sa.clone();
        test_sa.spec.credentials[1].ip_allow_list = vec![cidr.to_string()];
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

#[test]
fn test_has_api_key_check() {
    let mut sa = ServiceAccount::from_yaml(GOLDEN).expect("golden");
    assert!(sa.spec.has_api_key());

    // Remove the api-key credential
    sa.spec.credentials.truncate(1);
    assert!(!sa.spec.has_api_key());
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
