//! T-0176: a mirrored foreign model is not part of any local surface (DM-48, DM-49).
//!
//! The reconciler commits a peer's model into the repository beside the reference that fetched
//! it (`jcctl::foreign_models`), so the gateway loads it like every other manifest. DM-49 is
//! the line it must not cross: a foreign model is read-only and no local Endpoint may serve it
//! as its own. The endpoint table is where that is decided, so that is where it is tested.

use context_gateway::store;
use std::path::{Path, PathBuf};

const SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
spec:
  isSandbox: false
  urnSegment: ovzdusie
"#;

const ENDPOINT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: public-air
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  enabledRepresentations: ["ngsi-ld"]
"#;

/// The model this organisation authored and publishes.
const LOCAL: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: ovzdusie-air
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  linkml: ovzdusie-air/model.linkml.yaml
  version: 1.0.0
  lifecycle: published
  classes: [AirQualityObserved]
  artifacts:
    jsonSchema: ovzdusie-air/model.schema.json
    context: ovzdusie-air/context.jsonld
    docs: ovzdusie-air/model.md
    example: ovzdusie-air/example.jsonld
"#;

/// A partner's model, mirrored into the same space by the reconciler (DM-48).
const MIRRORED: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: partner-air
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  linkml: partner-air/model.linkml.yaml
  version: 2.1.0
  lifecycle: mirrored
  classes: [AirQualityObserved, WeatherObserved]
  source:
    remote:
      url: https://peer.example/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y/schema/v2/model.linkml.yaml
      version: 2.1.0
      sha256: 3b1f8e2c9a7d4b6e0f5a2c8d1e4b7a9c3f6d0b2e5a8c1d4f7b0e3a6c9d2f5b8e
      fetchedAt: 2026-09-07T08:00:00Z
  artifacts:
    context: partner-air/context.jsonld
"#;

fn repo(test: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-foreign-{test}-{now}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the repository");
    write(&dir, "space.yaml", SPACE);
    write(&dir, "endpoint.yaml", ENDPOINT);
    write(&dir, "local-model.yaml", LOCAL);
    write(&dir, "mirrored-model.yaml", MIRRORED);
    dir
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("write a manifest");
}

/// DM-49: the endpoint publishes the model this organisation authored, and only that one.
#[test]
fn an_endpoint_does_not_serve_a_foreign_model_as_its_own() {
    let dir = repo("endpoint");
    let (endpoints, _spaces, _accounts, _federations, _agreements) =
        store::load(&dir).expect("the repository loads");

    let endpoint = endpoints
        .iter()
        .find(|endpoint| endpoint.slug == "zt4qm7ge2xdv6ksb3ncf5arw2y")
        .expect("the endpoint is in the table");
    let names: Vec<&str> = endpoint
        .models
        .iter()
        .map(|model| model.name.as_str())
        .collect();
    assert_eq!(names, vec!["ovzdusie-air"]);
    // The types of the mirror do not leak in through the model either: `WeatherObserved` is
    // the partner's, and this endpoint has never claimed it.
    let types: Vec<&String> = endpoint
        .models
        .iter()
        .flat_map(|model| model.classes.iter())
        .collect();
    assert_eq!(types, vec!["AirQualityObserved"]);
}

/// The canonical space surface publishes what the space holds, which is the same answer: a
/// mirror is a copy of somebody else's model, not one of this space's own (SP-04, DM-49).
#[test]
fn the_space_surface_does_not_publish_a_mirror_either() {
    let dir = repo("space");
    let (_endpoints, spaces, _accounts, _federations, _agreements) =
        store::load(&dir).expect("the repository loads");

    let space = spaces
        .iter()
        .find(|space| space.endpoint.space == "ovzdusie")
        .expect("the space is in the table");
    let names: Vec<&str> = space
        .endpoint
        .models
        .iter()
        .map(|model| model.name.as_str())
        .collect();
    assert_eq!(names, vec!["ovzdusie-air"]);
}

/// Without the exclusion the mirror is an ordinary manifest, so the fixture has to prove the
/// repository really carries it: a test that passed because the file failed to load would say
/// nothing about DM-49.
#[test]
fn the_repository_really_holds_the_mirror() {
    let dir = repo("fixture");
    let repository = jcctl::loader::Repository::load(&dir).expect("the repository loads");
    let (_, resource) = repository
        .iter()
        .find(|(id, _)| id.kind == "DataModel" && id.name == "partner-air")
        .expect("the mirror is a loaded resource");
    let spec: jc_core::kinds::DataModelSpec =
        serde_json::from_value(resource.manifest.spec.clone()).expect("it parses as a DataModel");
    assert!(spec.validate().is_ok(), "{:?}", spec.validate());
    assert_eq!(spec.lifecycle, jc_core::kinds::DataModelLifecycle::Mirrored);
}
