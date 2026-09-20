use jc_core::error::{Error, ProblemDetails, UrnError, PROBLEM_JSON, PROBLEM_TYPE_BASE};
use serde_json::json;

#[test]
fn test_portal_api_not_found_byte_compatible() {
    let documented_json = json!({
        "type": "https://joinedcontext.com/errors/resource-not-found",
        "title": "Resource Not Found",
        "status": 404,
        "detail": "ContextSpace 'mobility-traffic' does not exist in project 'city-center'",
        "instance": "/api/v1/projects/city-center/spaces/mobility-traffic"
    });

    let prob = ProblemDetails::not_found()
        .with_detail("ContextSpace 'mobility-traffic' does not exist in project 'city-center'")
        .with_instance("/api/v1/projects/city-center/spaces/mobility-traffic");

    let serialized = serde_json::to_value(&prob).expect("serialization succeeds");
    assert_eq!(serialized, documented_json);
}

#[test]
fn test_all_constructors_set_status_and_type_base() {
    let cases = vec![
        (
            ProblemDetails::not_found(),
            404,
            "resource-not-found",
            "Resource Not Found",
        ),
        (
            ProblemDetails::forbidden(),
            403,
            "forbidden",
            "Access Denied by Policy",
        ),
        (
            ProblemDetails::bad_request(),
            400,
            "bad-request",
            "Bad Request",
        ),
        (ProblemDetails::conflict(), 409, "conflict", "Conflict"),
        (
            ProblemDetails::internal(),
            500,
            "internal-error",
            "Internal Server Error",
        ),
        (
            ProblemDetails::urn_scheme(),
            400,
            "urn-scheme",
            "Entity Identifier Violates the URN Scheme",
        ),
        (
            ProblemDetails::unauthorized(),
            401,
            "unauthorized",
            "Authentication Required",
        ),
    ];

    for (pd, status, slug, title) in cases {
        assert_eq!(pd.status, status);
        assert_eq!(pd.title, title);
        assert_eq!(pd.type_uri, format!("{PROBLEM_TYPE_BASE}{slug}"));
        assert!(pd.type_uri.starts_with(PROBLEM_TYPE_BASE));
        assert!(pd.detail.is_none());
        assert!(pd.instance.is_none());
        assert!(pd.extensions.is_empty());
    }
}

#[test]
fn test_internal_opaque_safety() {
    let prob = ProblemDetails::internal_opaque("req-94820-a8f");
    assert_eq!(prob.status, 500);
    assert_eq!(
        prob.type_uri,
        "https://joinedcontext.com/errors/internal-error"
    );
    assert!(prob.detail.is_some());

    let val = serde_json::to_value(&prob).expect("serialization succeeds");
    let json_str = serde_json::to_string(&prob).expect("to string succeeds");

    assert!(!json_str.to_lowercase().contains("panic"));
    assert!(!json_str.to_lowercase().contains("stack"));

    let obj = val.as_object().expect("object json");
    for k in obj.keys() {
        assert!(
            matches!(
                k.as_str(),
                "type" | "title" | "status" | "detail" | "instance" | "requestId"
            ),
            "unexpected field `{k}` in internal_opaque response"
        );
    }

    assert_eq!(obj.get("requestId"), Some(&json!("req-94820-a8f")));
    assert_eq!(obj.len(), 5);
}

#[test]
fn test_not_found_byte_identical() {
    let a = ProblemDetails::not_found();
    let b = ProblemDetails::not_found();
    let str_a = serde_json::to_string(&a).expect("serialize a");
    let str_b = serde_json::to_string(&b).expect("serialize b");
    assert_eq!(str_a, str_b);
}

#[test]
fn test_extensions_round_trip() {
    let prob = ProblemDetails::bad_request()
        .with_detail("invalid parameters supplied")
        .with_extension("field", json!("targetEndpoint"))
        .with_extension("retryAfter", json!(30));

    let serialized = serde_json::to_string(&prob).expect("serialize");
    let deserialized: ProblemDetails = serde_json::from_str(&serialized).expect("deserialize");

    assert_eq!(prob, deserialized);
    assert_eq!(
        deserialized.extensions.get("field"),
        Some(&json!("targetEndpoint"))
    );
    assert_eq!(deserialized.extensions.get("retryAfter"), Some(&json!(30)));
}

#[test]
fn test_from_error_mappings() {
    let urn_err = Error::Urn {
        urn: "urn:ngsi-ld:bad".to_string(),
        reason: UrnError::InvalidPrefix,
    };
    let pd_urn: ProblemDetails = urn_err.into();
    assert_eq!(pd_urn.status, 400);
    assert_eq!(
        pd_urn.type_uri,
        "https://joinedcontext.com/errors/urn-scheme"
    );
    assert_eq!(pd_urn.title, "Entity Identifier Violates the URN Scheme");
    assert!(pd_urn.detail.is_some());

    let name_err = Error::Name {
        field: "metadata.name",
        value: "INVALID_UPPERCASE".to_string(),
        reason: "must match DNS-1123 label regex",
    };
    let pd_name: ProblemDetails = name_err.into();
    assert_eq!(pd_name.status, 400);
    assert_eq!(
        pd_name.type_uri,
        "https://joinedcontext.com/errors/bad-request"
    );
    assert_eq!(pd_name.title, "Bad Request");
    assert!(pd_name.detail.is_some());
}

#[test]
fn test_problem_json_constant() {
    assert_eq!(PROBLEM_JSON, "application/problem+json");
}
