//! One principal per pipeline (PL-19, PL-20, T-1508, T-1509, Architecture/12 §3).
//!
//! Every Pipeline runs as a ServiceAccount nobody writes: `pl-{pipeline}`, derived here from the
//! Pipeline, bound to a Kubernetes ServiceAccount of its own in the runner's namespace, so its
//! federated Keycloak client `{project}-pl-{pipeline}` (PF-47) is told apart from every other
//! pipeline's by its subject. Its Policies are derived with it and grant what its outputs write
//! and its Endpoint source reads, over the declared type, and nothing else.
//!
//! The gateway (from the loaded repository) and the Portal (from its mirror) both derive through
//! [`derive()`], so the principal the gateway decides for is the one whose client the Portal writes.

use crate::envelope::{Ref, TypedRef};
use crate::kinds::pipeline::PipelineSpec;
use crate::kinds::pipeline_source_deletes::SourceDeleteMode;
use crate::kinds::policy::{
    EntitySelector, Operation, OperationGroup, OperationRef, PolicyEffect, PolicySpec, Principal,
    PrincipalKind, RegistrationInfo,
};
use crate::kinds::service_account::{
    Credential, CredentialKind, KubernetesBinding, Owner, RoleBinding, RoleScope,
    ServiceAccountSpec, Workload,
};
use sha2::{Digest, Sha256};

/// The name prefix of a derived account: no ServiceAccount manifest may take it (PL-19).
pub const ACCOUNT_PREFIX: &str = "pl-";

/// The role every derived binding carries; the Policies, not the role, hold the grants.
pub const ROLE: &str = "pipeline";

/// The longest name a DNS-1123 label allows.
const MAX: usize = 63;
/// How many hex digits of the SHA-256 end a name that had to be shortened.
const SUFFIX: usize = 10;

/// `name` as a DNS-1123 label: unchanged up to 63 characters, otherwise its first 52 and 10 hex
/// digits of the whole name's SHA-256, so it stays stable and one per pipeline (as MF-02).
pub fn bounded(name: &str) -> String {
    if name.len() <= MAX {
        return name.to_owned();
    }
    // Names here are ASCII DNS labels joined by hyphens, so byte offsets are characters.
    let prefix = name[..MAX - SUFFIX - 1].trim_end_matches('-');
    let digest = format!("{:x}", Sha256::digest(name.as_bytes()));
    format!("{prefix}-{}", &digest[..SUFFIX])
}

/// The derived account of a pipeline, `pl-{pipeline}`.
pub fn account_name(pipeline: &str) -> String {
    bounded(&format!("{ACCOUNT_PREFIX}{pipeline}"))
}

/// The Kubernetes ServiceAccount the account is bound to in the runner's namespace,
/// `pl-{project}-{pipeline}`: one per pipeline of the organization, since the runner is shared.
pub fn kubernetes_service_account(project: &str, pipeline: &str) -> String {
    bounded(&format!("{ACCOUNT_PREFIX}{project}-{pipeline}"))
}

/// Whether a ServiceAccount name is one only a Pipeline may have (PL-19).
pub fn is_derived(name: &str) -> bool {
    name.starts_with(ACCOUNT_PREFIX)
}

/// The name of each project's pipeline-test harness principal (PL-43, T-4050): the Portal tests an
/// unsaved spec and probes a DataSource as `{project}/pipeline-test` through the token service,
/// so the harness presents no client secret. Its account is [`account_name`] of it, its
/// Kubernetes ServiceAccount [`kubernetes_service_account`] of it, and its one audience the
/// Portal's internal listener. No Pipeline may take the name: it would share the harness's
/// principal.
pub const HARNESS: &str = "pipeline-test";

/// What one Pipeline brings with it: its principal and the Policies granted to it.
#[derive(Debug, Clone, PartialEq)]
pub struct Derived {
    /// The account's name, `pl-{pipeline}`.
    pub name: String,
    /// The account, bound to its own Kubernetes ServiceAccount.
    pub account: ServiceAccountSpec,
    /// The Policies, by name, each on one space.
    pub policies: Vec<(String, PolicySpec)>,
}

/// The principal and Policies of the pipeline `name` of `project` (PL-19, PL-20).
///
/// `space_of_endpoint` answers the space an Endpoint of the project serves, by the Endpoint's
/// name; an output or source whose Endpoint it does not know grants nothing, which the admission
/// check reports on the Pipeline itself (PL-55). `runner_namespace` is the deployment's.
pub fn derive(
    project: &str,
    name: &str,
    spec: &PipelineSpec,
    runner_namespace: &str,
    org_domain: &str,
    space_of_endpoint: impl Fn(&str) -> Option<String>,
) -> Derived {
    let account = account_name(name);
    let assignee = Principal::new(PrincipalKind::ServiceAccount, account.clone());
    let assigner = format!("did:web:{org_domain}");
    let mut policies = Vec::new();
    let mut spaces: Vec<String> = Vec::new();

    for (index, output) in spec.outputs().iter().enumerate() {
        // `urn:ngsi-ld:Endpoint:{org}:{space}:{name}`: the Endpoint's own name is the local id.
        let Some(space) = space_of_endpoint(output.target_endpoint.local_id()) else {
            continue;
        };
        let mut operations = vec![
            OperationRef::Single(Operation::UpsertBatch),
            OperationRef::Single(Operation::CreateBatch),
            OperationRef::Single(Operation::QueryBatch),
        ];
        // The age sweep and a snapshot or events rule delete through the pipeline's own token
        // (PL-64, PL-96); `lease` ends entities by `expiresAt`, which needs no delete.
        let deletes = spec.source_deletes.as_ref().is_some_and(|rule| {
            matches!(
                rule.mode,
                SourceDeleteMode::Snapshot | SourceDeleteMode::Events
            )
        });
        if spec.expiry.is_some() || deletes {
            operations.push(OperationRef::Single(Operation::DeleteBatch));
        }
        policies.push((
            bounded(&format!("{account}-w-{}", index + 1)),
            policy(
                &space,
                &assigner,
                &assignee,
                operations,
                output.entity_type.as_deref(),
            ),
        ));
        spaces.push(space);
    }

    // Either shape's sources (PL-54): `v1alpha1`'s one `source`, `v1alpha2`'s `sources`, of which
    // a `kpi` step reads 2 to 5 (PL-102). The first keeps the name `-r` it always had.
    for (index, source) in spec.sources().iter().enumerate() {
        if let Some(space) = source
            .endpoint_ref
            .as_ref()
            .and_then(|endpoint| space_of_endpoint(endpoint.name()))
        {
            let entity_type = source
                .query
                .as_ref()
                .and_then(|query| query.entity_type.as_deref());
            let name = match index {
                0 => format!("{account}-r"),
                n => format!("{account}-r-{}", n + 1),
            };
            policies.push((
                bounded(&name),
                policy(
                    &space,
                    &assigner,
                    &assignee,
                    vec![OperationRef::Group(OperationGroup::RetrieveOps)],
                    entity_type,
                ),
            ));
            spaces.push(space);
        }
    }

    spaces.sort();
    spaces.dedup();
    Derived {
        account: ServiceAccountSpec {
            owner: Owner {
                user: format!("pipeline/{project}/{name}"),
            },
            purpose: format!("The identity of the pipeline {project}/{name} (PL-19)."),
            // One binding per space it touches, so its client's audiences are those spaces'
            // Endpoints (PF-47); what it may do there is its Policies'.
            roles: spaces
                .into_iter()
                .map(|space| RoleBinding {
                    role: ROLE.to_owned(),
                    scope: RoleScope {
                        context_space: Some(space),
                        ..RoleScope::default()
                    },
                    types: Vec::new(),
                    operations: Vec::new(),
                    validity: None,
                })
                .collect(),
            credentials: vec![Credential {
                kind: CredentialKind::OauthClient,
                name: "federated".to_owned(),
                expires_at: None,
                ip_allow_list: Vec::new(),
                jwks: None,
            }],
            limits: None,
            workload: Some(Workload {
                kubernetes: KubernetesBinding {
                    namespace: runner_namespace.to_owned(),
                    service_account: kubernetes_service_account(project, name),
                },
            }),
            delegation: None,
        },
        name: account,
        policies,
    }
}

fn policy(
    space: &str,
    assigner: &str,
    assignee: &Principal,
    operations: Vec<OperationRef>,
    entity_type: Option<&str>,
) -> PolicySpec {
    PolicySpec {
        rules: None,
        context_space_ref: Ref::Typed(TypedRef {
            kind: "ContextSpace".to_owned(),
            name: space.to_owned(),
            namespace: None,
        }),
        effect: PolicyEffect::default(),
        assigner: assigner.to_owned(),
        assignee: assignee.clone(),
        operations,
        // A pipeline that names no type writes the classes of its space's model, which the
        // runner's validation stage holds it to (PL-60); one that names a type is held to it.
        information: entity_type
            .map(|entity_type| {
                vec![RegistrationInfo {
                    entities: vec![EntitySelector {
                        entity_type: entity_type.to_owned(),
                        id: None,
                        id_pattern: None,
                    }],
                    property_names: Vec::new(),
                    relationship_names: Vec::new(),
                }]
            })
            .unwrap_or_default(),
        q: None,
        scope_q: None,
        geo_q: None,
        temporal_q: None,
        validity: None,
        actions: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipeline(yaml: &str) -> PipelineSpec {
        serde_norway::from_str(yaml).expect("a pipeline spec")
    }

    fn spaces(endpoint: &str) -> Option<String> {
        match endpoint {
            "zilina-uniza" => Some("zilina-uniza".to_owned()),
            "zilina-mesto" => Some("zilina-mesto".to_owned()),
            "zilina-kpi" => Some("zilina-kpi".to_owned()),
            _ => None,
        }
    }

    const DREPO: &str = r#"
class: scheduled
schedule: "23 3 * * 2"
source: { dataSourceRef: { kind: DataSource, name: uniza-drepo } }
targetEndpoint: urn:ngsi-ld:Endpoint:zilina.sk:zilina-uniza:zilina-uniza
output: { type: CreativeWork, mode: upsert }
expiry: { after: 21d, types: [CreativeWork] }
"#;

    /// T-4050: the harness is a pipeline name like any other to the token service, so its account
    /// and Kubernetes ServiceAccount follow the same derivation.
    #[test]
    fn the_harness_principal_is_derived_like_a_pipeline() {
        assert_eq!(account_name(HARNESS), "pl-pipeline-test");
        assert_eq!(
            kubernetes_service_account("helsinki", HARNESS),
            "pl-helsinki-pipeline-test"
        );
        assert!(crate::names::validate_dns1123_label(HARNESS).is_ok());
    }

    /// PL-19: the principal, its Kubernetes ServiceAccount and the namespace are the pipeline's
    /// own, and the client derived from the name is `{project}-pl-{pipeline}`.
    #[test]
    fn a_pipeline_is_its_own_principal_bound_to_its_own_kubernetes_account() {
        let derived = derive(
            "zilina",
            "drepo",
            &pipeline(DREPO),
            "pipeline-runner",
            "zilina.sk",
            spaces,
        );
        assert_eq!(derived.name, "pl-drepo");
        assert_eq!(
            crate::kinds::service_account::keycloak_client_id("zilina", &derived.name),
            "zilina-pl-drepo"
        );
        let binding = &derived.account.workload.as_ref().expect("bound").kubernetes;
        assert_eq!(
            (binding.namespace.as_str(), binding.service_account.as_str()),
            ("pipeline-runner", "pl-zilina-drepo")
        );
        assert!(derived
            .account
            .credentials
            .iter()
            .all(|c| c.kind == CredentialKind::OauthClient));
    }

    /// PL-102, F-R2PLT3-008: a `v1alpha2` pipeline reads each Endpoint source as itself, so each
    /// gets its read grant; the first keeps the `-r` a `v1alpha1` source has always had.
    #[test]
    fn every_endpoint_source_of_either_shape_gets_its_read_grant() {
        let kpi = pipeline(
            r#"
class: scheduled
schedule: "0 4 * * *"
sources:
  - { name: unis, endpointRef: { kind: Endpoint, name: zilina-uniza }, query: { type: University } }
  - { name: town, endpointRef: { kind: Endpoint, name: zilina-mesto }, query: { type: District } }
outputs: [{ targetEndpoint: "urn:ngsi-ld:Endpoint:zilina.sk:zilina-kpi:zilina-kpi" }]
"#,
        );
        let derived = derive(
            "zilina",
            "kpi",
            &kpi,
            "pipeline-runner",
            "zilina.sk",
            spaces,
        );
        let grants: Vec<(&str, &str)> = derived
            .policies
            .iter()
            .map(|(name, policy)| (name.as_str(), policy.context_space_ref.name()))
            .collect();
        assert_eq!(
            grants,
            [
                ("pl-kpi-w-1", "zilina-kpi"),
                ("pl-kpi-r", "zilina-uniza"),
                ("pl-kpi-r-2", "zilina-mesto"),
            ]
        );
        assert_eq!(derived.account.roles.len(), 3);
    }

    /// PL-96, F-R2PLT3-009: a snapshot or events rule deletes through the pipeline's own token,
    /// so its write grant carries `deleteBatch`; `never` and `lease` delete nothing.
    #[test]
    fn a_source_delete_rule_that_deletes_is_granted_the_delete() {
        let without_expiry = DREPO.replace("expiry: { after: 21d, types: [CreativeWork] }\n", "");
        for (rule, deletes) in [
            ("{ mode: snapshot }", true),
            ("{ mode: events, deletedWhen: 'this.gone' }", true),
            ("{ mode: never }", false),
            ("{ mode: lease }", false),
        ] {
            let spec = pipeline(&format!("{without_expiry}sourceDeletes: {rule}\n"));
            let derived = derive(
                "zilina",
                "drepo",
                &spec,
                "pipeline-runner",
                "zilina.sk",
                spaces,
            );
            assert_eq!(
                derived.policies[0]
                    .1
                    .operations
                    .contains(&OperationRef::Single(Operation::DeleteBatch)),
                deletes,
                "{rule}"
            );
        }
    }

    /// PL-73, PL-55: a route writes only where the pipeline is granted, so each route output
    /// gets its write policy after the default lane's, and the lane's keeps its name.
    #[test]
    fn every_route_output_gets_its_own_write_grant() {
        let routed = pipeline(
            r#"
class: resident
sources: [{ dataSourceRef: { kind: DataSource, name: uniza-drepo } }]
routes:
  - name: kpi
    when: this.kpi == true
    outputs: [{ targetEndpoint: "urn:ngsi-ld:Endpoint:zilina.sk:zilina-kpi:zilina-kpi" }]
outputs: [{ targetEndpoint: "urn:ngsi-ld:Endpoint:zilina.sk:zilina-uniza:zilina-uniza" }]
"#,
        );
        let derived = derive(
            "zilina",
            "drepo",
            &routed,
            "pipeline-runner",
            "zilina.sk",
            spaces,
        );
        let grants: Vec<(&str, &str)> = derived
            .policies
            .iter()
            .map(|(name, policy)| (name.as_str(), policy.context_space_ref.name()))
            .collect();
        assert_eq!(
            grants,
            [
                ("pl-drepo-w-1", "zilina-uniza"),
                ("pl-drepo-w-2", "zilina-kpi")
            ]
        );
    }

    /// PL-20: the write grant is on the output's space over its type, with the sweep's delete
    /// only because the pipeline expires; nothing on any other space.
    #[test]
    fn the_derived_policy_writes_the_outputs_type_in_its_space_and_nothing_else() {
        let derived = derive(
            "zilina",
            "drepo",
            &pipeline(DREPO),
            "pipeline-runner",
            "zilina.sk",
            spaces,
        );
        let [(name, write)] = derived.policies.as_slice() else {
            panic!("one policy: {:?}", derived.policies);
        };
        assert_eq!(name, "pl-drepo-w-1");
        assert_eq!(write.context_space_ref.name(), "zilina-uniza");
        assert_eq!(
            write.assignee,
            Principal::new(PrincipalKind::ServiceAccount, "pl-drepo")
        );
        assert_eq!(write.assigner, "did:web:zilina.sk");
        let operations: Vec<&str> = write.operations.iter().map(OperationRef::as_str).collect();
        assert_eq!(
            operations,
            ["upsertBatch", "createBatch", "queryBatch", "deleteBatch"]
        );
        assert_eq!(write.information[0].entities[0].entity_type, "CreativeWork");
        assert_eq!(derived.account.roles.len(), 1);
        assert_eq!(
            derived.account.roles[0].scope.context_space.as_deref(),
            Some("zilina-uniza")
        );

        let without_expiry = DREPO.replace("expiry: { after: 21d, types: [CreativeWork] }\n", "");
        let derived = derive(
            "zilina",
            "drepo",
            &pipeline(&without_expiry),
            "pipeline-runner",
            "zilina.sk",
            spaces,
        );
        assert!(!derived.policies[0]
            .1
            .operations
            .contains(&OperationRef::Single(Operation::DeleteBatch)));
    }

    /// PL-20: an Endpoint source is read over its query's type; an Endpoint the project does not
    /// hold grants nothing rather than something on a guessed space.
    #[test]
    fn an_endpoint_source_is_read_and_an_unknown_endpoint_grants_nothing() {
        let kpi = r#"
class: scheduled
schedule: "17 5 * * 1"
source:
  endpointRef: { kind: Endpoint, name: zilina-mesto }
  query: { type: StatisticalObservation }
targetEndpoint: urn:ngsi-ld:Endpoint:zilina.sk:zilina-kpi:zilina-kpi
output: { type: KeyPerformanceIndicator, mode: upsert }
compute: { kind: bloblang, bloblang: "root = this" }
"#;
        let derived = derive(
            "zilina",
            "ukazovatele",
            &pipeline(kpi),
            "pipeline-runner",
            "zilina.sk",
            spaces,
        );
        let names: Vec<&str> = derived
            .policies
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names, ["pl-ukazovatele-w-1", "pl-ukazovatele-r"]);
        let read = &derived.policies[1].1;
        assert_eq!(read.context_space_ref.name(), "zilina-mesto");
        assert_eq!(
            read.operations,
            [OperationRef::Group(OperationGroup::RetrieveOps)]
        );
        assert_eq!(
            read.information[0].entities[0].entity_type,
            "StatisticalObservation"
        );

        let elsewhere = DREPO.replace("zilina-uniza:zilina-uniza", "zilina-uniza:someone-elses");
        assert!(derive(
            "zilina",
            "drepo",
            &pipeline(&elsewhere),
            "pipeline-runner",
            "zilina.sk",
            spaces
        )
        .policies
        .is_empty());
    }

    /// PL-19: a name past a DNS label's 63 characters is shortened the same way every time and
    /// stays one per pipeline.
    #[test]
    fn a_long_name_is_bounded_stable_and_distinct() {
        let long = "a".repeat(40);
        let one = kubernetes_service_account(&"p".repeat(40), &long);
        assert_eq!(one.len(), 63);
        assert_eq!(one, kubernetes_service_account(&"p".repeat(40), &long));
        assert_ne!(
            one,
            kubernetes_service_account(&"p".repeat(40), &format!("{long}b"))
        );
        assert_eq!(account_name("drepo"), "pl-drepo");
        assert!(is_derived("pl-drepo") && !is_derived("pipelines"));
    }
}
