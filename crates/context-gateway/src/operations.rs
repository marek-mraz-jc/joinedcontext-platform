//! Which NGSI-LD operation a request is (T-0005, GW4, R8).
//!
//! A `Policy` grants operations from the CIM 009 vocabulary, so the gateway has to name
//! the operation before it can ask the PDP anything. The mapping is the resource tree of
//! ETSI GS CIM 009 clause 5, and it is deliberately total in one direction only: a path
//! and method pair this table does not know is not an NGSI-LD operation, and the gateway
//! answers 404 rather than guessing which grant might cover it.

use axum::http::Method;
use jc_core::kinds::Operation;

/// The NGSI-LD operation a request on the `ngsi-ld/v1` surface performs.
///
/// `path` is the part after `/ngsi-ld/v1/`, without a leading slash. `details` is the
/// `details` query parameter, which is what separates the two type-listing operations.
pub fn operation_of(method: &Method, path: &str, details: bool) -> Option<Operation> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match (segments.as_slice(), method) {
        (["entities"], &Method::GET) => Some(Operation::QueryEntity),
        (["entities"], &Method::POST) => Some(Operation::CreateEntity),
        (["entities", _], &Method::GET) => Some(Operation::RetrieveEntity),
        (["entities", _], &Method::PATCH) => Some(Operation::MergeEntity),
        (["entities", _], &Method::PUT) => Some(Operation::ReplaceEntity),
        (["entities", _], &Method::DELETE) => Some(Operation::DeleteEntity),
        (["entities", _, "attrs"], &Method::POST) => Some(Operation::AppendAttrs),
        (["entities", _, "attrs"], &Method::PATCH) => Some(Operation::UpdateAttrs),
        (["entities", _, "attrs", _], &Method::PATCH) => Some(Operation::UpdateAttrs),
        (["entities", _, "attrs", _], &Method::PUT) => Some(Operation::ReplaceAttrs),
        (["entities", _, "attrs", _], &Method::DELETE) => Some(Operation::DeleteAttrs),

        (["entityOperations", "create"], &Method::POST) => Some(Operation::CreateBatch),
        (["entityOperations", "upsert"], &Method::POST) => Some(Operation::UpsertBatch),
        (["entityOperations", "update"], &Method::POST) => Some(Operation::UpdateBatch),
        (["entityOperations", "merge"], &Method::POST) => Some(Operation::MergeBatch),
        (["entityOperations", "delete"], &Method::POST) => Some(Operation::DeleteBatch),
        (["entityOperations", "query"], &Method::POST) => Some(Operation::QueryBatch),

        (["temporal", "entities"], &Method::GET) => Some(Operation::QueryTemporal),
        // CIM 009 clause 5.6.12: the POST form of Query Temporal Evolution asks the same
        // question with the selector in the body, so it is the same operation and the same
        // grant. The body is narrowed where the entity batch's is (`app.rs`).
        (["temporal", "entityOperations", "query"], &Method::POST) => {
            Some(Operation::QueryTemporal)
        }
        (["temporal", "entities"], &Method::POST) => Some(Operation::UpsertTemporal),
        (["temporal", "entities", _], &Method::GET) => Some(Operation::RetrieveTemporal),
        (["temporal", "entities", _], &Method::DELETE) => Some(Operation::DeleteTemporal),
        (["temporal", "entities", _, "attrs"], &Method::POST) => {
            Some(Operation::AppendAttrsTemporal)
        }
        (["temporal", "entities", _, "attrs", _], &Method::DELETE) => {
            Some(Operation::DeleteAttrsTemporal)
        }
        (["temporal", "entities", _, "attrs", _, _], &Method::PATCH) => {
            Some(Operation::UpdateAttrInstanceTemporal)
        }
        (["temporal", "entities", _, "attrs", _, _], &Method::DELETE) => {
            Some(Operation::DeleteAttrInstanceTemporal)
        }

        (["types"], &Method::GET) if details => Some(Operation::RetrieveEntityTypeDetails),
        (["types"], &Method::GET) => Some(Operation::RetrieveEntityTypes),
        (["types", _], &Method::GET) => Some(Operation::RetrieveEntityTypeInfo),
        (["attributes"], &Method::GET) if details => Some(Operation::RetrieveAttrTypeDetails),
        (["attributes"], &Method::GET) => Some(Operation::RetrieveAttrTypes),
        (["attributes", _], &Method::GET) => Some(Operation::RetrieveAttrTypeInfo),

        (["subscriptions"], &Method::GET) => Some(Operation::QuerySubscription),
        (["subscriptions"], &Method::POST) => Some(Operation::CreateSubscription),
        (["subscriptions", _], &Method::GET) => Some(Operation::RetrieveSubscription),
        (["subscriptions", _], &Method::PATCH) => Some(Operation::UpdateSubscription),
        (["subscriptions", _], &Method::DELETE) => Some(Operation::DeleteSubscription),

        _ => None,
    }
}

/// The entity identifier a single-resource path names, if it names one.
///
/// A refused read of one entity answers 404 rather than 403, so the gateway has to know
/// whether the request was about one entity at all (GW1, R20).
pub fn addressed_entity(path: &str) -> Option<&str> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["entities", id, ..] => Some(id),
        ["temporal", "entities", id, ..] => Some(id),
        _ => None,
    }
}

/// The one type or attribute name a discovery path addresses, when it addresses one.
///
/// `GET /types/{type}` and `GET /attributes/{attr}` are the two reads that name a member of the
/// vocabulary rather than an entity, and whether the caller may learn that the name exists is
/// decided before the broker is asked (EP-25, T-2134).
pub fn addressed_vocabulary(path: &str) -> Option<&str> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["types", name] | ["attributes", name] => Some(name),
        _ => None,
    }
}

/// The single attribute a path addresses, when it addresses one.
///
/// The body of such a write is the bare attribute value, so the enforcement point has to
/// learn the attribute's name from the path before it can ask whether the grant covers it
/// (GW17).
pub fn targeted_attribute(path: &str) -> Option<&str> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["entities", _, "attrs", attr] => Some(attr),
        ["temporal", "entities", _, "attrs", attr, ..] => Some(attr),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_paths_the_demo_uses_map_to_the_operations_a_policy_grants() {
        for (method, path, expected) in [
            (Method::GET, "entities", Operation::QueryEntity),
            (
                Method::GET,
                "entities/urn:ngsi-ld:X:a:b:c",
                Operation::RetrieveEntity,
            ),
            (Method::POST, "entities", Operation::CreateEntity),
            (
                Method::PATCH,
                "entities/urn:x/attrs",
                Operation::UpdateAttrs,
            ),
            (Method::DELETE, "entities/urn:x", Operation::DeleteEntity),
            (Method::GET, "temporal/entities", Operation::QueryTemporal),
        ] {
            assert_eq!(
                operation_of(&method, path, false),
                Some(expected),
                "{method} /{path}"
            );
        }
    }

    #[test]
    fn a_path_or_method_outside_the_vocabulary_names_no_operation() {
        for (method, path) in [
            (Method::GET, "entities/urn:x/attrs/temperature"),
            (Method::POST, "entities/urn:x"),
            (Method::GET, "../../admin"),
            (Method::GET, ""),
            (Method::HEAD, "entities"),
            (Method::GET, "entityOperations/query"),
        ] {
            assert_eq!(operation_of(&method, path, false), None, "{method} /{path}");
        }
    }

    #[test]
    fn details_separates_the_two_type_listing_operations() {
        assert_eq!(
            operation_of(&Method::GET, "types", false),
            Some(Operation::RetrieveEntityTypes)
        );
        assert_eq!(
            operation_of(&Method::GET, "types", true),
            Some(Operation::RetrieveEntityTypeDetails)
        );
    }

    #[test]
    fn a_single_attribute_path_names_the_attribute_it_addresses() {
        assert_eq!(
            targeted_attribute("entities/urn:x/attrs/pm10"),
            Some("pm10")
        );
        assert_eq!(
            targeted_attribute("temporal/entities/urn:x/attrs/pm10/inst-1"),
            Some("pm10")
        );
        assert_eq!(targeted_attribute("entities/urn:x/attrs"), None);
        assert_eq!(targeted_attribute("entities/urn:x"), None);
    }

    #[test]
    fn a_single_entity_path_names_the_entity_it_addresses() {
        assert_eq!(
            addressed_entity("entities/urn:ngsi-ld:X:a:b:c"),
            Some("urn:ngsi-ld:X:a:b:c")
        );
        assert_eq!(addressed_entity("entities/urn:x/attrs/pm10"), Some("urn:x"));
        assert_eq!(addressed_entity("temporal/entities/urn:x"), Some("urn:x"));
        assert_eq!(addressed_entity("entities"), None);
        assert_eq!(addressed_entity("subscriptions/urn:sub"), None);
    }
}
