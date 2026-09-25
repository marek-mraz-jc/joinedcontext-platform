//! The manifest kinds of `apiVersion: joinedcontext.com/v1alpha1` (MF-01, Architecture/06).

pub mod agent_profile;
pub mod app;
pub mod bento_inputs;
pub mod bento_processors;
pub mod blueprint;
pub mod catalog;
pub mod ckan;
pub mod csr;
pub mod dashboard;
pub mod data_model;
pub mod data_source;
pub mod dataspace;
pub mod endpoint;
pub mod environment;
pub mod grid;
pub mod group;
pub mod mapping;
pub mod model_projection;
pub mod organization;
pub mod pipeline;
pub mod policy;
pub mod role;
pub mod service_account;
pub mod space;
pub mod subscription;
pub mod sync;
pub mod ui_schema;

pub use agent_profile::{
    AgentEgress, AgentLimits, AgentModel, AgentProfileRole, AgentProfileSpec, AgentRuntime,
    AgentTool, AgentWorkspace, ModelProvider, DEFAULT_EGRESS_BYTES_PER_RUN,
};
pub use app::{
    AppAccess, AppBuild, AppClass, AppLifecycle, AppLimits, AppRole, AppSource, AppSpec,
    AppVisibility, ContentSecurityPolicy, DataNeed, GeoConstraint, GeoWithin, GitSource,
    TemporalConstraint,
};
pub use blueprint::{BlueprintSpec, BlueprintTemplate, RiskClass};
pub use catalog::{
    Catalog, ContactPoint, DataTheme, Duty, Frequency, Licence, Publisher, Source, Temporal,
};
pub use ckan::{CkanInstanceSpec, CkanPublication, DataStore, DataStoreRefresh, Publication};
pub use csr::{ContextSourceRegistrationSpec, Federation, FederationIdentity, RegistrationMode};
pub use dashboard::{
    ColorBy, DashboardSpec, DashboardVisibility, LayerFilter, LayerSpec, LayerStyle, Page, SizeBy,
    Widget,
};
pub use data_model::{
    DataModelLifecycle, DataModelSource, DataModelSpec, GeneratedArtifacts, RemoteSource, SemVer,
};
pub use data_source::{
    check_class, Authorization, DataSourceSpec, DataSourceType, GtfsFeed, GtfsRtConnection,
    HttpConnection, MqttConnection, TlsSettings, WebSocketConnection,
};
pub use dataspace::{
    AgreementConstraints, AgreementRole, AgreementState, ConnectorEngine, DataAgreementSpec,
    DataOfferSpec, DataSpaceParticipantSpec, Did,
};
pub use endpoint::{
    endpoint_role, Audience, Caching, EndpointRef, EndpointRole, EndpointSlug, EndpointSpec,
    FileLimits, Projection, RateLimits, Representation, SharedSpaceReferenceSpec,
    ENDPOINT_ROLE_PREFIX,
};
pub use environment::{EnvironmentSpec, SecretBackend};
pub use group::{GroupSpec, Member};
pub use mapping::{
    DataModelRef, MappingArtifacts, MappingSpec, MappingTest, NativeBlock, NativeLanguage,
    VocabularyAlignment,
};
pub use model_projection::{ModelProjectionSpec, ProjectedClass, ProjectionFilter};
pub use organization::{
    Contact, ContactRole, OrganizationSpec, ProjectsPolicy, DEFAULT_NAME_COOLDOWN_DAYS,
};
pub use pipeline::{
    Compute, ComputeKind, Output, OutputMode, PipelineClass, PipelineOutput, PipelineQuotas,
    PipelineSource, PipelineSpec, ProcessorStep, SourceQuery, Step, SubscriptionTrigger,
    TemporalWindow, Trigger,
};
pub use policy::{
    EntitySelector, Operation, OperationGroup, OperationRef, PolicyEffect, PolicySpec, Principal,
    PrincipalKind, RegistrationInfo, ScopeDefinitionSpec, Validity,
};
pub use role::{BindingValidity, Constraint, RoleBindingSpec, RoleSpec, Rule, Subject, Verb};
pub use service_account::{
    Credential, CredentialKind, KubernetesBinding, Owner, RoleBinding, RoleScope,
    ServiceAccountLimits, ServiceAccountSpec, Workload,
};
pub use space::{urn_segment, ContextSpaceSpec, ProjectSpec, Quotas};
pub use subscription::{
    KeyValue, Notification, NotificationEndpoint, SubscriptionSpec, WatchedEntity,
};
pub use sync::{
    BundleFile, BundleItem, BundleOrigin, BundleRepository, BundleRole, BundleSchemas, BundleSpec,
    ConflictPolicy, GitOrigin, PlatformApiOrigin, Schedule, SyncMode, SyncOrigin, SyncSourceSpec,
    WebhookAuth,
};
pub use ui_schema::{UiSchemaField, UiSchemaGroup, UiSchemaSpec};

/// `kind: Organization` as a whole manifest.
pub type Organization = crate::envelope::ResourceEnvelope<OrganizationSpec>;
/// `kind: Project` as a whole manifest.
pub type Project = crate::envelope::ResourceEnvelope<ProjectSpec>;
/// `kind: ContextSpace` as a whole manifest.
pub type ContextSpace = crate::envelope::ResourceEnvelope<ContextSpaceSpec>;
/// `kind: Endpoint` as a whole manifest.
pub type Endpoint = crate::envelope::ResourceEnvelope<EndpointSpec>;
/// `kind: SharedSpaceReference` as a whole manifest.
pub type SharedSpaceReference = crate::envelope::ResourceEnvelope<SharedSpaceReferenceSpec>;
/// `kind: Policy` as a whole manifest.
pub type Policy = crate::envelope::ResourceEnvelope<PolicySpec>;
/// `kind: ScopeDefinition` as a whole manifest.
pub type ScopeDefinition = crate::envelope::ResourceEnvelope<ScopeDefinitionSpec>;
/// `kind: ServiceAccount` as a whole manifest.
pub type ServiceAccount = crate::envelope::ResourceEnvelope<ServiceAccountSpec>;
/// `kind: Pipeline` as a whole manifest.
pub type Pipeline = crate::envelope::ResourceEnvelope<PipelineSpec>;
/// `kind: DataModel` as a whole manifest.
pub type DataModel = crate::envelope::ResourceEnvelope<DataModelSpec>;
/// `kind: Mapping` as a whole manifest.
pub type Mapping = crate::envelope::ResourceEnvelope<MappingSpec>;
/// `kind: App` as a whole manifest.
pub type App = crate::envelope::ResourceEnvelope<AppSpec>;

/// `kind: CkanInstance` as a whole manifest.
pub type CkanInstance = crate::envelope::ResourceEnvelope<CkanInstanceSpec>;
/// `kind: Blueprint` as a whole manifest.
pub type Blueprint = crate::envelope::ResourceEnvelope<BlueprintSpec>;
/// `kind: AgentProfile` as a whole manifest.
pub type AgentProfile = crate::envelope::ResourceEnvelope<AgentProfileSpec>;
/// `kind: SyncSource` as a whole manifest.
pub type SyncSource = crate::envelope::ResourceEnvelope<SyncSourceSpec>;
/// `kind: Bundle` as a whole manifest.
pub type Bundle = crate::envelope::ResourceEnvelope<BundleSpec>;
/// `kind: DataSpaceParticipant` as a whole manifest.
pub type DataSpaceParticipant = crate::envelope::ResourceEnvelope<DataSpaceParticipantSpec>;
/// `kind: DataOffer` as a whole manifest.
pub type DataOffer = crate::envelope::ResourceEnvelope<DataOfferSpec>;
/// `kind: DataAgreement` as a whole manifest.
pub type DataAgreement = crate::envelope::ResourceEnvelope<DataAgreementSpec>;
/// `kind: Role` as a whole manifest. (`kind: RoleBinding` has no alias: `RoleBinding` is the
/// grant inside a ServiceAccount; use `ResourceEnvelope<RoleBindingSpec>`.)
pub type Role = crate::envelope::ResourceEnvelope<RoleSpec>;
/// `kind: Dashboard` as a whole manifest.
pub type Dashboard = crate::envelope::ResourceEnvelope<DashboardSpec>;
/// `kind: Layer` as a whole manifest.
pub type Layer = crate::envelope::ResourceEnvelope<LayerSpec>;
/// `kind: UiSchema` as a whole manifest.
pub type UiSchema = crate::envelope::ResourceEnvelope<UiSchemaSpec>;
/// `kind: Environment` as a whole manifest.
pub type Environment = crate::envelope::ResourceEnvelope<EnvironmentSpec>;

/// `kind: Group` as a whole manifest.
pub type Group = crate::envelope::ResourceEnvelope<GroupSpec>;

/// The default mirror interval of DM-49, in seconds: 24 hours.
pub const MIRROR_INTERVAL_DEFAULT_SECONDS: u64 = 24 * 60 * 60;

/// Checks the schedule a reference kind mirrors a peer's schema surface on (DM-49).
///
/// Absent is the 24-hour default, so absence is valid. A webhook schedule is not: a peer in
/// another organisation has no reason to call us, and a schedule that can never fire would
/// stop mirroring silently rather than say so. An interval this crate cannot parse is refused
/// for the same reason — it would fall back to the default and look deliberate.
pub fn validate_mirror_schedule(schedule: Option<&Schedule>) -> crate::error::Result<()> {
    let Some(schedule) = schedule else {
        return Ok(());
    };
    if schedule.webhook == Some(true) {
        return Err(crate::error::Error::Name {
            field: "spec.schedule.webhook",
            value: "true".to_owned(),
            reason: "a mirrored peer sends no webhook; give spec.schedule.interval instead",
        });
    }
    match schedule.interval.as_deref() {
        None => Err(crate::error::Error::Name {
            field: "spec.schedule",
            value: String::new(),
            reason: "a schedule with no interval never fires; leave it out for the 24h default",
        }),
        Some(interval) if schedule.interval_seconds().is_none() => Err(crate::error::Error::Name {
            field: "spec.schedule.interval",
            value: interval.to_owned(),
            reason: "an interval is a number and one of the suffixes s, m, h, d",
        }),
        Some(_) => Ok(()),
    }
}
