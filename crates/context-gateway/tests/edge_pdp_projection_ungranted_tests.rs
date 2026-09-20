//! Edge cases of `pdp::projection::ungranted` (T-1931, EP-26, MP-02).
//!
//! Contract, in one sentence: given a write payload and the grant's attribute whitelist, it names
//! every member of that payload the whitelist does not cover — identity members excepted — and it
//! names them all, because the write guard denies the write whole rather than trimming it (GW17).
//!
//! Two silences would be defects and both are tested here: a member it fails to name is a member
//! a caller writes without a grant, and an empty answer for a payload it cannot read is a write
//! nobody checked. The second is why `check_attributes` is never the only check on a write path.

use context_gateway::pdp::projection::ungranted;
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn granted(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// GW17: the broker's own members are not a client's to send, so a payload carrying one is
/// outside the grant like any other attribute. `project_entity` keeps them on a **read**; this is
/// the other direction and it deliberately does not know that list.
#[test]
fn a_write_that_carries_a_broker_generated_member_is_not_granted_it() {
    let entity = json!({
        "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1",
        "type": "Vehicle",
        "weight": 1,
        "createdAt": "2020-01-01T00:00:00Z",
        "modifiedAt": "2020-01-01T00:00:00Z",
        "deletedAt": "2020-01-01T00:00:00Z",
        "expiresAt": "2020-01-01T00:00:00Z"
    });

    assert_eq!(
        ungranted(&entity, &granted(&["weight"])),
        vec!["createdAt", "deletedAt", "expiresAt", "modifiedAt"],
        "a client that sets a timestamp is writing outside its grant"
    );
}

/// A grant with no whitelist is a grant over the whole entity: nothing is outside it, so the
/// write guard has nothing to refuse.
#[test]
fn an_empty_whitelist_puts_nothing_outside_the_grant() {
    for entity in [
        json!({ "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1", "type": "Vehicle", "weight": 1 }),
        json!({ "anything": 1, "at": 2, "all": 3 }),
        json!({}),
    ] {
        assert!(ungranted(&entity, &BTreeSet::new()).is_empty(), "{entity}");
    }
}

/// The identity members are never "outside the grant": a write has to carry them, and refusing a
/// payload for naming its own `id` would refuse every write.
#[test]
fn the_identity_members_are_never_named() {
    let entity = json!({
        "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1",
        "@id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1",
        "type": "Vehicle",
        "@type": "Vehicle",
        "@context": "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld",
        "scope": "/fleet/depot-1"
    });

    assert!(ungranted(&entity, &granted(&["weight"])).is_empty());
}

/// Every ungranted member is named, not the first one: the refusal reports one, but a caller that
/// fixes it must not find a second the check never mentioned.
#[test]
fn every_member_outside_the_grant_is_named() {
    let entity = json!({
        "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1",
        "type": "Vehicle",
        "weight": 1, "plate": 2, "owner": 3, "location": 4
    });

    assert_eq!(
        ungranted(&entity, &granted(&["weight"])),
        vec!["location", "owner", "plate"]
    );
    assert!(ungranted(&entity, &granted(&["weight", "plate", "owner", "location"])).is_empty());
}

/// A name is compared exactly: neither case, nor a stray space, nor a percent-encoded or
/// look-alike spelling makes a member granted.
#[test]
fn a_name_is_compared_exactly() {
    let entity = json!({
        "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1",
        "type": "Vehicle",
        "Weight": 1, "weight ": 2, " weight": 3, "weight%20": 4, "we%69ght": 5, "ｗeight": 6
    });

    let outside = ungranted(&entity, &granted(&["weight"]));
    assert_eq!(outside.len(), 6, "none of these is `weight`: {outside:?}");
}

/// EP-26: a payload the function cannot read has nothing outside the grant, which is not the same
/// as being allowed. `app.rs` unwraps a batch into its entities and refuses a body that is not an
/// object before this is asked, and `check_no_smuggled_policy` guards the same shape — struck as
/// impossible on the write path by `app.rs:915` (`for entity in entities`) and `app.rs:947`.
#[test]
fn a_payload_that_is_not_an_object_names_nothing_and_decides_nothing() {
    for payload in [
        json!([{ "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1", "weight": 1 }]),
        json!("urn:ngsi-ld:Vehicle:hel.fi:fleet:1"),
        json!(0),
        Value::Null,
        json!(true),
    ] {
        assert!(
            ungranted(&payload, &granted(&["nothing"])).is_empty(),
            "{payload}"
        );
    }
}

/// The names come back sorted, so a refusal reads the same for the same payload whatever order
/// the client serialised its members in.
#[test]
fn the_names_are_stable_whatever_order_the_client_wrote_them_in() {
    let one: Value = serde_json::from_str(
        r#"{"id":"urn:ngsi-ld:Vehicle:hel.fi:fleet:1","owner":1,"plate":2,"weight":3}"#,
    )
    .expect("valid json");
    let other: Value = serde_json::from_str(
        r#"{"weight":3,"plate":2,"owner":1,"id":"urn:ngsi-ld:Vehicle:hel.fi:fleet:1"}"#,
    )
    .expect("valid json");

    assert_eq!(
        ungranted(&one, &granted(&["weight"])),
        vec!["owner", "plate"]
    );
    assert_eq!(
        ungranted(&other, &granted(&["weight"])),
        vec!["owner", "plate"]
    );
}

/// A duplicated member is one member: JSON keeps the last, so a caller cannot smuggle a second
/// value past a check that saw the first.
#[test]
fn a_duplicated_member_is_named_once() {
    let entity: Value = serde_json::from_str(
        r#"{"id":"urn:ngsi-ld:Vehicle:hel.fi:fleet:1","plate":"AA-111","plate":"BB-222"}"#,
    )
    .expect("valid json");

    assert_eq!(ungranted(&entity, &granted(&["weight"])), vec!["plate"]);
    assert_eq!(
        entity["plate"],
        json!("BB-222"),
        "the last one is what would be written"
    );
}

/// A name carrying control characters is reported as written; escaping it belongs to whatever
/// renders the refusal, and the JSON body of a Problem Details does that by construction.
#[test]
fn a_name_with_control_characters_comes_back_as_written() {
    let entity = json!({
        "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1",
        "weight\r\nSet-Cookie: a=b": 1,
        "": 2,
        "\u{0}": 3
    });

    let outside = ungranted(&entity, &granted(&["weight"]));
    assert_eq!(outside, vec!["", "\u{0}", "weight\r\nSet-Cookie: a=b"]);
    // And the refusal these names travel in is JSON, where a newline cannot end a header.
    let rendered = serde_json::to_string(&json!({ "detail": outside[2] })).expect("serialises");
    assert!(!rendered.contains('\n'), "{rendered}");
}

/// An endpoint's hidden attributes are not this function's business: EP-61 is enforced before the
/// write guard is called (`app.rs:936`), and a whitelist that named the hidden attribute would
/// otherwise let the write through here. Struck as impossible at this level, tested at that one.
#[test]
fn a_hidden_attribute_inside_the_grant_is_not_named_here() {
    let entity = json!({ "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1", "operatorPhone": "+421 900" });
    assert!(ungranted(&entity, &granted(&["operatorPhone"])).is_empty());
}

/// A large payload is checked whole: the hundredth attribute is named like the first.
#[test]
fn every_attribute_of_a_large_payload_is_checked() {
    let mut entity = json!({ "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1", "type": "Vehicle" });
    let members = entity.as_object_mut().expect("an object");
    for index in 0..256 {
        members.insert(format!("attr{index:03}"), json!(index));
    }

    let outside = ungranted(&entity, &granted(&["attr000"]));
    assert_eq!(outside.len(), 255);
    assert!(
        outside.contains(&"attr255"),
        "the last attribute is checked too"
    );
    assert!(!outside.contains(&"attr000"));
}

/// Asking does not change the payload, so the write that is forwarded is the one that was judged.
#[test]
fn asking_leaves_the_payload_untouched() {
    let entity = json!({ "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1", "weight": 1, "plate": 2 });
    let before = entity.clone();
    assert_eq!(ungranted(&entity, &granted(&["weight"])), vec!["plate"]);
    assert_eq!(entity, before);
}
