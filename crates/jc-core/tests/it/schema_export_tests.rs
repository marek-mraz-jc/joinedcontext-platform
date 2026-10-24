//! T-0122: every catalogued kind exports a JSON Schema that is draft-07 and nothing newer.

use jc_core::registry::{self, KINDS};

/// Keywords introduced by draft 2019-09 / 2020-12; react-jsonschema-form rejects them (CC-12, DM-03).
const POST_DRAFT07_KEYWORDS: &[&str] = &[
    "$defs",
    "$dynamicRef",
    "$dynamicAnchor",
    "$recursiveRef",
    "$recursiveAnchor",
    "prefixItems",
    "unevaluatedProperties",
    "unevaluatedItems",
    "dependentSchemas",
    "dependentRequired",
];

#[test]
fn every_kind_exports_a_draft07_schema() {
    assert!(!KINDS.is_empty());
    for info in KINDS {
        let schema = registry::schema_of(info.kind)
            .unwrap_or_else(|| panic!("no schema for kind {}", info.kind));
        assert_eq!(
            schema.get("$schema").and_then(|v| v.as_str()),
            Some("http://json-schema.org/draft-07/schema#"),
            "{} must declare the draft-07 dialect",
            info.kind
        );
        let text = serde_json::to_string(&schema).expect("schema serializes");
        for keyword in POST_DRAFT07_KEYWORDS {
            assert!(
                !text.contains(&format!("\"{keyword}\"")),
                "{} schema uses post-draft-07 keyword {keyword}",
                info.kind
            );
        }
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap_or_else(|| panic!("{} schema has no properties", info.kind));
        for required in ["apiVersion", "kind", "metadata", "spec"] {
            assert!(
                props.contains_key(required),
                "{} schema is missing the envelope property {required}",
                info.kind
            );
        }
    }
}

#[test]
fn schema_of_rejects_an_unknown_kind() {
    assert!(registry::schema_of("NoSuchKind").is_none());
}

/// The committed `schemas/kinds/*.json` must equal what `export_schemas` produces (DM-02 pattern):
/// the Portal forms and `jcctl validate` read the committed copies, so drift is a defect.
#[test]
fn committed_schemas_match_the_generator() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("schemas/kinds");

    for info in KINDS {
        let path = root.join(format!("{}.json", info.kind));
        let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} — run `cargo run -p jcctl -- schema export`",
                path.display()
            )
        });
        let mut generated =
            serde_json::to_string_pretty(&registry::schema_of(info.kind).expect("schema"))
                .expect("serialize");
        generated.push('\n');
        assert_eq!(
            committed,
            generated,
            "{} is stale — run `cargo run -p jcctl -- schema export`",
            path.display()
        );
    }
}
