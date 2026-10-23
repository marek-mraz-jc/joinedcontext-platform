//! T-0563: an endpoint that references a `ModelProjection` reads the projected subset of
//! the model and nothing wider (MP-02, MP-03).
//!
//! The grant says who may read; the projection says what the endpoint is about; the
//! gateway intersects the two before any representation. What is worth a test: an
//! attribute outside the projection never reaches the wire, a type outside it is refused
//! before the broker is asked, the residual filter is conjoined so a caller cannot widen
//! it, a grant narrower than the projection wins, and the schema surface describes the
//! projected model only.

mod common;

use context_gateway::auth::subject::Subject;
use context_gateway::handlers::schema;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::{Audience, ModelProjectionSpec, PolicySpec, Representation};
use std::collections::BTreeSet;
use std::sync::Arc;

const SLUG: &str = "3ecozggnnhjlp5miouhia53mr2";
const PROJECT: &str = "helsinki";
const SPACE: &str = "fleet";
const DOMAIN: &str = "hel.fi";

const PARTNER_VIEW: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ModelProjection
metadata:
  name: partner-view
  namespace: helsinki
spec:
  contextSpaceRef: fleet
  dataModelRef: { kind: DataModel, name: fleet, version: "1" }
  classes:
    - name: Vehicle
      slots: [name, speed]
    - name: User
      slots: [name, age]
  filter:
    q: category=="public"
"#;

fn projection() -> Arc<ModelProjectionSpec> {
    let parsed = ResourceEnvelope::<ModelProjectionSpec>::from_yaml(PARTNER_VIEW).expect("parses");
    parsed.validate().expect("valid");
    Arc::new(parsed.spec)
}

fn policy(information: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity]\n\
         {information}"
    ))
    .expect("the policy spec parses")
}

fn endpoint(policy: PolicySpec, projection: Option<Arc<ModelProjectionSpec>>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: SPACE.to_owned(),
        project: PROJECT.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection,
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["Vehicle".to_owned(), "User".to_owned(), "Depot".to_owned()],
            json_schema: None,
            context: None,
        }],
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy],
    }
}

/// MP-03: the projection narrows the types the gateway names (the schema documents themselves
/// are the broker's since T-3646, `schema_view_tests.rs`).
#[test]
fn the_schema_surface_serves_the_projected_model() {
    let now = context_gateway::pdp::now();
    let subject = Subject::anonymous();

    let unprojected = endpoint(policy(""), None);
    let visible = schema::visible(&subject, &unprojected, now);
    let types: BTreeSet<String> = schema::visible_types(&unprojected, &visible)
        .into_iter()
        .collect();
    assert!(types.contains("Depot"), "{types:?}");

    let projected = endpoint(policy(""), Some(projection()));
    let visible = schema::visible(&subject, &projected, now);
    let types: BTreeSet<String> = schema::visible_types(&projected, &visible)
        .into_iter()
        .collect();
    assert_eq!(
        types,
        BTreeSet::from(["Vehicle".to_owned(), "User".to_owned()]),
        "exactly the projected classes"
    );
}
