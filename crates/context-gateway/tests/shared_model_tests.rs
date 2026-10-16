//! T-4352: a reload reads and compiles each space's model once; every Endpoint of the space and
//! the space's own surface hold that one copy (owner 2026-10-16: the gateway's memory).

use context_gateway::store;
use std::path::Path;
use std::sync::Arc;

const MODEL: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: bb-air-quality
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  linkml: bb-air-quality.linkml.yaml
  version: 1.0.0
  lifecycle: draft
  classes: [AirQualityObserved]
  artifacts:
    jsonSchema: bb-air-quality.schema.json
    context: bb-air-quality.context.jsonld
"#;

fn endpoint(name: &str, slug: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: {name}\n  namespace: ovzdusie\n\
         spec:\n  contextSpaceRef: ovzdusie\n  slug: {slug}\n  audience: public\n  enabledRepresentations: [\"ngsi-ld\"]\n"
    )
}

#[test]
fn every_record_of_a_space_shares_one_parsed_model() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-shared-model-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    let write =
        |name: &str, body: &str| std::fs::write(Path::new(&dir).join(name), body).expect("write");
    write(
        "space.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: ovzdusie\n  namespace: ovzdusie\n\
         spec:\n  urnSegment: ovzdusie\n  dataModelRef: { kind: DataModel, name: bb-air-quality }\n",
    );
    write("model.yaml", MODEL);
    write(
        "bb-air-quality.schema.json",
        r#"{"$defs": {"AirQualityObserved": {"properties": {"pm10": {"type": "object"}}}}}"#,
    );
    write(
        "bb-air-quality.context.jsonld",
        r#"{"@context": {"@vocab": "https://bb.example.sk/"}}"#,
    );
    write("one.yaml", &endpoint("one", "aaaaaaaaaaaaaaaaaaaaaaaaaa"));
    write("two.yaml", &endpoint("two", "zt4qm7ge2xdv6ksb3ncf5arw2y"));

    let (endpoints, spaces, ..) = store::load(&dir).expect("the repository loads");
    let _ = std::fs::remove_dir_all(&dir);
    let [one, two] = endpoints.as_slice() else {
        panic!("two endpoints, got {}", endpoints.len());
    };
    let space = &spaces.first().expect("the space").endpoint;

    let schema = |models: &[context_gateway::resolver::Model]| {
        Arc::clone(models[0].json_schema.as_ref().expect("the schema is read"))
    };
    let context = |models: &[context_gateway::resolver::Model]| {
        Arc::clone(models[0].context.as_ref().expect("the context is read"))
    };
    assert!(Arc::ptr_eq(&schema(&one.models), &schema(&two.models)));
    assert!(Arc::ptr_eq(&schema(&one.models), &schema(&space.models)));
    assert!(Arc::ptr_eq(&context(&one.models), &context(&two.models)));
    assert!(Arc::ptr_eq(&context(&one.models), &context(&space.models)));

    let declared = |endpoint: &context_gateway::resolver::Endpoint| {
        Arc::clone(
            endpoint
                .declared_types
                .as_ref()
                .expect("the space names its model"),
        )
    };
    assert!(Arc::ptr_eq(&declared(one), &declared(two)));
    assert!(Arc::ptr_eq(&declared(one), &declared(space)));
    // The shared copy is the model as read: nothing is dropped by sharing it.
    assert_eq!(
        declared(one).classes,
        ["AirQualityObserved".to_owned()].into()
    );
    assert!(schema(&one.models)["$defs"]["AirQualityObserved"].is_object());
}
