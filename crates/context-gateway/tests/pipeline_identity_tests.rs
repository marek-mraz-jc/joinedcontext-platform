//! Every Pipeline is its own principal at the gateway (PL-19, PL-20, T-1508, T-1509).
//!
//! The account `pl-{pipeline}` and its Policies are derived from the Pipeline when the
//! repository is loaded, never written: a token of the pipeline's federated client
//! `{project}-pl-{pipeline}` resolves to that account, and the endpoints of the space it writes
//! carry its write grant over the type it declares, while the space it does not touch carries
//! nothing of it.

use context_gateway::store;
use jc_core::kinds::{OperationRef, PrincipalKind};
use std::path::PathBuf;

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
                endpoint("events-all", "events", "eeeeeeeeeeeeeeeeeeeeeeeeee"),
            ),
            (
                "parking-all.yaml",
                endpoint("parking-all", "parking", "pppppppppppppppppppppppppp"),
            ),
            ("pipeline.yaml", PIPELINE.to_owned()),
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
                    && policy.assignee.id == "pl-linked-events"
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
