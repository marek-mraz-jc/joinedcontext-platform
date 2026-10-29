//! Edge cases of `auth::token::Claims::roles` (T-1899, EP-26, MP-02).
//!
//! Contract, in one sentence: the roles a token asserts are exactly the strings in
//! `realm_access.roles`, as the realm wrote them — never a group, never a resource role,
//! never a role inferred from any other claim — and a token that asserts none says so with an
//! empty slice rather than with an absence the caller has to handle (PF-46).
//!
//! It is thirteen lines with one branch, and that is the point: everything that decides what a
//! caller may do reads this, so what it must never do is invent a role or drop one. The cases
//! here are the shapes Keycloak's claim really takes, plus the ones a forged token would use.

use context_gateway::auth::token::Claims;
use serde_json::{json, Value};

fn claims_of(value: Value) -> Claims {
    serde_json::from_value(value).expect("the claims parse")
}

/// The claims of a token, with whatever `realm_access` the case needs put on top.
fn token(extra: Value) -> Claims {
    let mut value = json!({
        "iss": "https://id.example/realms/joinedcontext",
        "sub": "f:1:mm",
        "exp": 1_800_000_000_i64,
    });
    for (key, member) in extra.as_object().expect("an object") {
        value[key] = member.clone();
    }
    claims_of(value)
}

#[test]
fn a_token_that_asserts_no_realm_access_asserts_no_role() {
    assert!(token(json!({})).roles().is_empty());
}

#[test]
fn a_realm_access_with_no_roles_member_asserts_no_role() {
    assert!(token(json!({ "realm_access": {} })).roles().is_empty());
    assert!(token(json!({ "realm_access": { "roles": [] } }))
        .roles()
        .is_empty());
}

/// A realm role that is `null`, a number or an object is not a string, and `realm_access`
/// stops parsing rather than half-reading the list: a token whose roles cannot be read
/// asserts nothing, which is the safe half of the two.
#[test]
fn a_roles_list_that_is_not_a_list_of_strings_asserts_nothing() {
    for shape in [
        json!({ "realm_access": { "roles": "platform-admin" } }),
        json!({ "realm_access": { "roles": [null] } }),
        json!({ "realm_access": { "roles": [42] } }),
        json!({ "realm_access": { "roles": [{ "name": "platform-admin" }] } }),
        json!({ "realm_access": { "roles": ["ok", 42] } }),
        json!({ "realm_access": "platform-admin" }),
        json!({ "realm_access": null }),
    ] {
        let mut value = json!({
            "iss": "https://id.example/realms/joinedcontext",
            "sub": "f:1:mm",
            "exp": 1_800_000_000_i64,
        });
        value["realm_access"] = shape["realm_access"].clone();
        let roles = serde_json::from_value::<Claims>(value)
            .map(|claims| claims.roles().to_vec())
            .unwrap_or_default();
        assert!(
            roles.is_empty(),
            "{shape} must not assert a role, and it asserts {roles:?}",
        );
    }
}

#[test]
fn the_roles_are_the_strings_the_realm_wrote_in_the_order_it_wrote_them() {
    let claims = token(json!({
        "realm_access": { "roles": ["space-writer", "default-roles-joinedcontext", "platform-admin"] }
    }));
    assert_eq!(
        claims.roles(),
        [
            "space-writer",
            "default-roles-joinedcontext",
            "platform-admin"
        ],
    );
}

/// Nothing is trimmed, folded or split: a role is compared elsewhere by equality, so a value
/// that merely looks like `platform-admin` must arrive looking like itself.
#[test]
fn no_role_is_trimmed_folded_or_split() {
    let odd = [
        " platform-admin",
        "platform-admin ",
        "PLATFORM-ADMIN",
        "platform-admin\n",
        "platform-admin\0",
        "platform-admin,space-writer",
        "platform-admin space-writer",
        "platform%2Dadmin",
        "рlatform-admin",
        "",
    ];
    let claims = token(json!({ "realm_access": { "roles": odd } }));
    assert_eq!(claims.roles(), odd);
    assert!(
        !claims.roles().iter().any(|role| role == "platform-admin"),
        "not one of them is the role itself",
    );
}

#[test]
fn a_role_repeated_is_kept_repeated_rather_than_deduplicated_here() {
    let claims = token(json!({ "realm_access": { "roles": ["viewer", "viewer", "viewer"] } }));
    assert_eq!(claims.roles().len(), 3, "the claim is reported, not tidied");
}

#[test]
fn a_long_list_of_roles_is_reported_whole() {
    let many: Vec<String> = (0..256).map(|n| format!("role-{n}")).collect();
    let claims = token(json!({ "realm_access": { "roles": many } }));
    assert_eq!(claims.roles().len(), 256);
    assert_eq!(claims.roles()[255], "role-255");
}

/// Groups and roles are two claims and two things. A token whose groups look like roles
/// asserts no role at all, or a group membership would become a permission.
#[test]
fn a_group_is_never_read_as_a_role() {
    let claims = token(json!({
        "groups": ["/administrators", "platform-admin"],
        "realm_access": { "roles": ["viewer"] },
    }));
    assert_eq!(claims.roles(), ["viewer"]);
    assert_eq!(claims.groups, ["/administrators", "platform-admin"]);
}

/// Keycloak also publishes `resource_access`, where a client's own roles live. The gateway
/// declares no such member, and `Claims` does not deny unknown ones — so it is read past, and
/// a client role never becomes a realm role.
#[test]
fn a_resource_access_role_is_never_read_as_a_realm_role() {
    let claims = token(json!({
        "realm_access": { "roles": ["viewer"] },
        "resource_access": { "context-gateway": { "roles": ["platform-admin"] } },
    }));
    assert_eq!(claims.roles(), ["viewer"]);
}

/// The slice borrows the claims rather than copying them, so reading it twice is reading the
/// same thing: nothing here can be exhausted or consumed by a first caller.
#[test]
fn reading_the_roles_twice_reads_the_same_roles() {
    let claims = token(json!({ "realm_access": { "roles": ["viewer", "space-writer"] } }));
    let first: Vec<String> = claims.roles().to_vec();
    let second: Vec<String> = claims.roles().to_vec();
    assert_eq!(first, second);
    assert_eq!(first, ["viewer", "space-writer"]);
}

/// A transfer token carries the connector's own realm roles, and they are the connector's:
/// `dataspace_token::subject` drops them. This is the claim side of that property — `roles`
/// reports what the token says, and it is the caller of `roles` that must not ask for a
/// transfer token's roles at all (DS-02, T-1896).
#[test]
fn a_transfer_tokens_roles_are_reported_as_what_they_are_the_connectors() {
    let claims = token(json!({
        "azp": "banskabystrica-dataspace-connector",
        "agreementId": "urn:uuid:9a1f-air-quality",
        "participant": "did:web:helsinki.fi",
        "realm_access": { "roles": ["platform-admin"] },
    }));
    assert_eq!(claims.roles(), ["platform-admin"]);
    assert_eq!(
        claims.agreement_id.as_deref(),
        Some("urn:uuid:9a1f-air-quality")
    );
}
