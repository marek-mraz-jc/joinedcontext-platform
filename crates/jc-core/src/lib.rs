//! Shared domain and configuration-as-code models for the joinedcontext platform.
//!
//! This crate defines the deterministic URN scheme (PF-10, PF-42), Kubernetes-style
//! manifest envelope and metadata types (MF-01..MF-10), multi-language localization
//! mapping (PF-24..PF-28), typed reference schemas, and validation rules used across
//! the Context Gateway, reconciler (`jcctl`), and Portal backend.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod envelope;
pub mod error;
pub mod i18n;
pub mod kinds;
pub mod kpi;
pub mod names;
pub mod registry;
pub mod urn;

pub use envelope::{
    annotations, de_api_version, de_api_version_of, de_kind, serves, validate_locales, Build,
    Condition, Kind, ObjectMeta, Phase, Ref, ResourceEnvelope, Scope, SecretRef, Status, TypedRef,
    API_VERSION, API_VERSION_V1ALPHA2,
};
pub use error::{Error, ProblemDetails, Result, UrnError, PROBLEM_JSON, PROBLEM_TYPE_BASE};
pub use i18n::{MultiLanguageMap, Text};
pub use kinds::{
    AgentEgress, AgentLimits, AgentModel, AgentProfile, AgentProfileRole, AgentProfileSpec,
    AgentRuntime, AgentTool, AgentWorkspace, AgreementConstraints, AgreementRole, AgreementState,
    App, AppBuild, AppClass, AppLifecycle, AppLimits, AppSource, AppSpec, AppVisibility, Audience,
    Bundle, BundleItem, BundleOrigin, BundleSpec, Caching, CkanInstance, CkanInstanceSpec,
    CkanPublication, Compute, ComputeKind, ConflictPolicy, ConnectorEngine, Contact, ContactRole,
    ContentSecurityPolicy, ContextSpace, ContextSpaceSpec, Credential, CredentialKind, Dashboard,
    DashboardSpec, DashboardVisibility, DataAgreement, DataAgreementSpec, DataModel,
    DataModelLifecycle, DataModelRef, DataModelSource, DataModelSpec, DataNeed, DataOffer,
    DataOfferSpec, DataSpaceParticipant, DataSpaceParticipantSpec, Did, Endpoint, EndpointSlug,
    EndpointSpec, EntitySelector, Environment, EnvironmentSpec, FileLimits, GeneratedArtifacts,
    GeoConstraint, GeoWithin, GitOrigin, GitSource, KubernetesBinding, Layer, LayerFilter,
    LayerSpec, LayerStyle, Mapping, MappingSpec, MappingTest, ModelProvider, NativeBlock,
    NativeLanguage, Operation, OperationGroup, OperationRef, Organization, OrganizationSpec,
    Output, OutputMode, Owner, Pipeline, PipelineClass, PipelineQuotas, PipelineSource,
    PipelineSpec, PlatformApiOrigin, Policy, PolicyEffect, PolicySpec, Principal, PrincipalKind,
    Project, ProjectSpec, Projection, Quotas, RateLimits, RegistrationInfo, RemoteSource,
    Representation, Role, RoleBinding, RoleBindingSpec, RoleScope, RoleSpec, Schedule,
    ScopeDefinition, ScopeDefinitionSpec, SemVer, ServiceAccount, ServiceAccountLimits,
    ServiceAccountSpec, SharedSpaceReference, SharedSpaceReferenceSpec, SourceQuery,
    SubscriptionTrigger, SyncMode, SyncOrigin, SyncSource, SyncSourceSpec, TemporalConstraint,
    TemporalWindow, Trigger, Validity, VocabularyAlignment, Widget, Workload,
    DEFAULT_EGRESS_BYTES_PER_RUN,
};
pub use registry::{by_kind, by_plural, KindInfo, KINDS};
pub use urn::{apply_render_prefix, Urn};
