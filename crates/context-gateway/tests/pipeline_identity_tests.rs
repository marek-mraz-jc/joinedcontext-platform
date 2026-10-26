//! Every Pipeline is its own principal at the gateway (PL-19, PL-20, T-1508, T-1509).
//!
//! The account `pl-{pipeline}` and its Policies are derived from the Pipeline when the
//! repository is loaded, never written: a token of the pipeline's federated client
//! `{project}-pl-{pipeline}` resolves to that account, and the endpoints of the space it writes
//! carry its write grant over the type it declares, while the space it does not touch carries
//! nothing of it.

mod common;

use axum::body::Body;
use axum::http::Request;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use jc_core::kinds::{OperationRef, PrincipalKind};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

fn space(name: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: {name}\n  namespace: helsinki\nspec:\n  isSandbox: false\n  urnSegment: {name}\n"
    )
}

fn endpoint(name: &str, space: &str, slug: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: {name}\n  namespace: helsinki\nspec:\n  contextSpaceRef: {space}\n  slug: {slug}\n  audience: organization\n  enabledRepresentations: [ngsi-ld]\n"
    )
}

const PIPELINE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Pipeline
metadata:
  name: linked-events
  namespace: helsinki
spec:
  class: auto
  period: 30m
  schedule: "*/30 * * * *"
  source:
    dataSourceRef: { kind: DataSource, name: hel-linked-events }
  targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:events:events-all
  output:
    type: Event
    mode: upsert
"#;

/// The project the pipeline and both Endpoints live in, and its Organization (ADR-N-054 2.3).
const PROJECT: &str =
    "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  name: helsinki\n  \
    namespace: org\nspec:\n  organizationRef: { kind: Organization, name: hel }\n";

struct Repo(PathBuf);

impl Repo {
    fn new() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after the epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gw-pipeline-identity-{now}"));
        std::fs::create_dir_all(&dir).expect("a repository");
        for (file, body) in [
            ("events.yaml", space("events")),
            ("parking.yaml", space("parking")),
            (
                "events-all.yaml",
                endpoint("events-all", "events", "eeeeeeeeeeeeeeeeeeeeeeeeee")
                    + "  audit: changes\n",
            ),
            (
                "parking-all.yaml",
                endpoint("parking-all", "parking", "pppppppppppppppppppppppppp"),
            ),
            ("pipeline.yaml", PIPELINE.to_owned()),
            ("project.yaml", PROJECT.to_owned()),
        ] {
            std::fs::write(dir.join(file), body).expect("a manifest");
        }
        Self(dir)
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// PL-19: the pipeline's federated client resolves to its own account and to nothing wider: its
/// one role reaches the space it writes and not the other.
#[test]
fn a_pipelines_own_client_resolves_to_its_derived_account() {
    let repo = Repo::new();
    let (_, _, accounts, ..) = store::load(&repo.0).expect("the repository loads");
    let account = accounts
        .resolve("helsinki-pl-linked-events")
        .expect("the pipeline's client resolves");
    assert_eq!(
        (account.name.as_str(), account.project.as_str()),
        ("pl-linked-events", "helsinki")
    );
    assert!(!account.delegates);
    assert_eq!(account.roles_in("helsinki", "events").len(), 1);
    assert!(account.roles_in("helsinki", "parking").is_empty());
    assert!(accounts.resolve("helsinki-pl-something-else").is_none());
}

/// PL-20: the endpoint of the space the pipeline writes carries its write grant over the type it
/// declares, assigned to its account; the other space's endpoint carries nothing of it.
#[test]
fn the_written_spaces_endpoint_carries_the_pipelines_write_grant_and_no_other_does() {
    let repo = Repo::new();
    let (endpoints, ..) = store::load(&repo.0).expect("the repository loads");
    let of = |name: &str| {
        endpoints
            .iter()
            .find(|e| e.slug.starts_with(name))
            .expect("the endpoint")
    };
    let grants = |slug_head: &str| {
        of(slug_head)
            .policies
            .iter()
            .filter(|policy| {
                policy.assignee.kind == PrincipalKind::ServiceAccount
                    && policy.assignee.id == "helsinki/pl-linked-events"
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    let [write] = grants("eeee")
        .try_into()
        .expect("one grant on the written space");
    let operations: Vec<&str> = write.operations.iter().map(OperationRef::as_str).collect();
    assert_eq!(operations, ["upsertBatch", "createBatch", "queryBatch"]);
    assert_eq!(write.information[0].entities[0].entity_type, "Event");
    assert!(
        grants("pppp").is_empty(),
        "nothing on a space the pipeline does not write"
    );
}

/// ADR-N-054 2.3 (T-5361): the pipeline's write reaches the broker as channel `pipeline` with its
/// own account as the actor and one request id, whatever provenance the client typed.
#[tokio::test]
async fn a_pipelines_write_reaches_the_broker_as_channel_pipeline_with_its_account() {
    let repo = Repo::new();
    let tables = store::load(&repo.0).expect("the repository loads");
    let (endpoints, accounts, provenance) = (tables.0, tables.2, tables.11);
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let slug = "eeeeeeeeeeeeeeeeeeeeeeeeee";
    let gateway = Gateway::new(Broker::new(&broker.url), Box::new(PolicyPdp), "hel.fi")
        .serve(endpoints)
        .authenticate(Arc::new(realm.verifier()), accounts, None);
    gateway.replace_provenance(provenance);
    let token = realm.workload_token("helsinki-pl-linked-events", json!(slug));
    let entities = json!([{ "id": "urn:ngsi-ld:Event:t5361", "type": "Event" }]).to_string();
    let request = Request::post(format!(
        "/api/endpoint/{slug}/ngsi-ld/v1/entityOperations/upsert"
    ))
    .header("authorization", format!("Bearer {token}"))
    .header("content-type", "application/json")
    .header("jc-provenance-channel", "ui")
    .header("jc-provenance-actor", "user:someone-else")
    .header("jc-provenance-organization", "another-city")
    .header("jc-provenance-actor-kind", "person")
    .body(Body::from(entities))
    .expect("a request");
    let status = router(Arc::new(gateway))
        .oneshot(request)
        .await
        .expect("an answer")
        .status();

    let hops = broker.hops();
    assert_eq!(hops.len(), 1, "{status}: {hops:?}");
    let headers = &hops[0].headers;
    let values = |name: &str| {
        headers
            .get_all(name)
            .iter()
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(values("jc-provenance-channel"), ["pipeline"], "{headers:?}");
    assert_eq!(
        values("jc-provenance-actor"),
        ["serviceAccount%3Apl-linked-events"],
        "{headers:?}"
    );
    assert_eq!(
        values("x-request-id")
            .iter()
            .map(String::len)
            .collect::<Vec<_>>(),
        [36],
        "{headers:?}"
    );
    // DM-111, DM-136: the Organization of its Project, its kind, and the audited Endpoint's mark.
    assert_eq!(values("jc-provenance-organization"), ["hel"], "{headers:?}");
    assert_eq!(
        values("jc-provenance-actor-kind"),
        ["pipeline"],
        "{headers:?}"
    );
    assert_eq!(values("jc-provenance-audit"), ["changes"], "{headers:?}");
}

/// DM-136: only an Endpoint with `spec.audit: changes` is audited; the default is off.
#[test]
fn only_an_endpoint_that_says_so_is_audited() {
    let repo = Repo::new();
    let provenance = store::load(&repo.0).expect("the repository loads").11;
    assert_eq!(
        provenance.audited.iter().collect::<Vec<_>>(),
        ["eeeeeeeeeeeeeeeeeeeeeeeeee"]
    );
    assert_eq!(
        provenance.organizations.get("helsinki").map(String::as_str),
        Some("hel")
    );
}
