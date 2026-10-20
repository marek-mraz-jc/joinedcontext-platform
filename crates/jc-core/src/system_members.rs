//! The NGSI-LD system members (DM-130): the one list every checker reads, so none of them
//! counts a member as an Attribute (DM-131).
//!
//! A closed model shape admits these members, a Policy's attribute grant never needs to list
//! them and a revalidation never reports them. Only the exact terms are members: `ExpiresAt`
//! or `ngsi-ld:expiresAt` is an attribute name like any other and stays checked.

/// The members of an Entity that are no Attribute of it (DM-130).
pub const ENTITY_SYSTEM_MEMBERS: [&str; 8] = [
    "id",
    "type",
    "scope",
    "@context",
    "createdAt",
    "modifiedAt",
    "deletedAt",
    "expiresAt",
];

/// The members of an Attribute that are no sub-Attribute of it (DM-130), with the value members of
/// a JsonProperty, ListProperty and ListRelationship (TS 104 175 4.5).
pub const ATTRIBUTE_SYSTEM_MEMBERS: [&str; 16] = [
    "type",
    "value",
    "object",
    "objectType",
    "languageMap",
    "vocab",
    "json",
    "valueList",
    "objectList",
    "observedAt",
    "unitCode",
    "datasetId",
    "expiresAt",
    "createdAt",
    "modifiedAt",
    "instanceId",
];

/// The system members whose value must be an RFC 3339 instant (DM-131).
pub const INSTANT_MEMBERS: [&str; 5] = [
    "createdAt",
    "modifiedAt",
    "deletedAt",
    "expiresAt",
    "observedAt",
];

/// Whether `name` is an entity system member, or `@id` and `@type`, the JSON-LD keywords the
/// core context aliases `id` and `type` to.
pub fn is_entity_system_member(name: &str) -> bool {
    ENTITY_SYSTEM_MEMBERS.contains(&name) || matches!(name, "@id" | "@type")
}

/// Whether `name` is an attribute system member.
pub fn is_attribute_system_member(name: &str) -> bool {
    ATTRIBUTE_SYSTEM_MEMBERS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_terms_are_members() {
        for name in ENTITY_SYSTEM_MEMBERS.iter().chain(["@id", "@type"].iter()) {
            assert!(is_entity_system_member(name), "{name}");
        }
        for name in ATTRIBUTE_SYSTEM_MEMBERS {
            assert!(is_attribute_system_member(name), "{name}");
        }
        for name in [
            "ExpiresAt",
            "expiresat",
            "ngsi-ld:expiresAt",
            "https://uri.etsi.org/ngsi-ld/expiresAt",
            " expiresAt",
            "observedAt",
            "speed",
            "",
        ] {
            assert!(!is_entity_system_member(name), "{name}");
        }
        assert!(!is_attribute_system_member("ExpiresAt"));
        assert!(!is_attribute_system_member("scope"));
    }
}
