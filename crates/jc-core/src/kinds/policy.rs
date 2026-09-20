//! Manifest kinds for access policies and scope definitions (T-0114, ADR 002, ADR 005, R3, R6..R8).

use crate::envelope::{Kind, ObjectMeta, Ref, Scope};
use crate::error::{Error, Result};
use crate::names;
use crate::urn::Urn;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Table 4.20-1: every named distributed API operation in ETSI GS CIM 009 (R8).
///
/// Closed enum ensures no proprietary verbs (`upsertEntity`, `read`, `write`) are accepted,
/// satisfying the strict R8 CIM 009 vocabulary guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum Operation {
    /// Create entity (`POST /entities`).
    #[serde(rename = "createEntity")]
    CreateEntity,
    /// Update entity (`PATCH /entities/{id}`).
    #[serde(rename = "updateEntity")]
    UpdateEntity,
    /// Append entity attributes (`POST /entities/{id}/attrs`).
    #[serde(rename = "appendAttrs")]
    AppendAttrs,
    /// Update entity attributes (`PATCH /entities/{id}/attrs`).
    #[serde(rename = "updateAttrs")]
    UpdateAttrs,
    /// Delete entity attributes (`DELETE /entities/{id}/attrs/{attr}`).
    #[serde(rename = "deleteAttrs")]
    DeleteAttrs,
    /// Delete entity (`DELETE /entities/{id}`).
    #[serde(rename = "deleteEntity")]
    DeleteEntity,
    /// Batch create entities (`POST /entityOperations/create`).
    #[serde(rename = "createBatch")]
    CreateBatch,
    /// Batch upsert entities (`POST /entityOperations/upsert`).
    #[serde(rename = "upsertBatch")]
    UpsertBatch,
    /// Batch update entities (`POST /entityOperations/update`).
    #[serde(rename = "updateBatch")]
    UpdateBatch,
    /// Batch delete entities (`POST /entityOperations/delete`).
    #[serde(rename = "deleteBatch")]
    DeleteBatch,
    /// Upsert temporal entity attributes (`POST /temporal/entities/{id}/attrs`).
    #[serde(rename = "upsertTemporal")]
    UpsertTemporal,
    /// Append temporal entity attributes (`POST /temporal/entities/{id}/attrs`).
    #[serde(rename = "appendAttrsTemporal")]
    AppendAttrsTemporal,
    /// Delete temporal entity attributes (`DELETE /temporal/entities/{id}/attrs/{attr}`).
    #[serde(rename = "deleteAttrsTemporal")]
    DeleteAttrsTemporal,
    /// Update temporal attribute instance (`PATCH /temporal/entities/{id}/attrs/{attr}/{instance}`).
    #[serde(rename = "updateAttrInstanceTemporal")]
    UpdateAttrInstanceTemporal,
    /// Delete temporal attribute instance (`DELETE /temporal/entities/{id}/attrs/{attr}/{instance}`).
    #[serde(rename = "deleteAttrInstanceTemporal")]
    DeleteAttrInstanceTemporal,
    /// Delete temporal entity (`DELETE /temporal/entities/{id}`).
    #[serde(rename = "deleteTemporal")]
    DeleteTemporal,
    /// Merge entity (`PATCH /entities/{id}`).
    #[serde(rename = "mergeEntity")]
    MergeEntity,
    /// Replace entity (`PUT /entities/{id}`).
    #[serde(rename = "replaceEntity")]
    ReplaceEntity,
    /// Replace entity attributes (`PATCH /entities/{id}/attrs`).
    #[serde(rename = "replaceAttrs")]
    ReplaceAttrs,
    /// Batch merge entities (`POST /entityOperations/merge`).
    #[serde(rename = "mergeBatch")]
    MergeBatch,
    /// Purge entity permanently (`DELETE /entities/{id}?purge=true`).
    #[serde(rename = "purgeEntity")]
    PurgeEntity,
    /// Retrieve single entity (`GET /entities/{id}`).
    #[serde(rename = "retrieveEntity")]
    RetrieveEntity,
    /// Query entities (`GET /entities` or `POST /entityOperations/query`).
    #[serde(rename = "queryEntity")]
    QueryEntity,
    /// Batch query entities (`POST /entityOperations/query`).
    #[serde(rename = "queryBatch")]
    QueryBatch,
    /// Retrieve temporal entity (`GET /temporal/entities/{id}`).
    #[serde(rename = "retrieveTemporal")]
    RetrieveTemporal,
    /// Query temporal entities (`GET /temporal/entities`).
    #[serde(rename = "queryTemporal")]
    QueryTemporal,
    /// Retrieve entity types list (`GET /types`).
    #[serde(rename = "retrieveEntityTypes")]
    RetrieveEntityTypes,
    /// Retrieve details of the available entity types (`GET /types?details=true`).
    #[serde(rename = "retrieveEntityTypeDetails")]
    RetrieveEntityTypeDetails,
    /// Retrieve one entity type's information (`GET /types/{type}`).
    #[serde(rename = "retrieveEntityTypeInfo")]
    RetrieveEntityTypeInfo,
    /// Retrieve attribute types list (`GET /attributes`).
    #[serde(rename = "retrieveAttrTypes")]
    RetrieveAttrTypes,
    /// Retrieve details of the available attributes (`GET /attributes?details=true`).
    #[serde(rename = "retrieveAttrTypeDetails")]
    RetrieveAttrTypeDetails,
    /// Retrieve one attribute's information (`GET /attributes/{attr}`).
    #[serde(rename = "retrieveAttrTypeInfo")]
    RetrieveAttrTypeInfo,
    /// Create context subscription (`POST /subscriptions`).
    #[serde(rename = "createSubscription")]
    CreateSubscription,
    /// Update context subscription (`PATCH /subscriptions/{id}`).
    #[serde(rename = "updateSubscription")]
    UpdateSubscription,
    /// Retrieve context subscription (`GET /subscriptions/{id}`).
    #[serde(rename = "retrieveSubscription")]
    RetrieveSubscription,
    /// Query context subscriptions (`GET /subscriptions`).
    #[serde(rename = "querySubscription")]
    QuerySubscription,
    /// Delete context subscription (`DELETE /subscriptions/{id}`).
    #[serde(rename = "deleteSubscription")]
    DeleteSubscription,
    /// Retrieve context entity map (`GET /entityMaps/{id}`).
    #[serde(rename = "retrieveEntityMap")]
    RetrieveEntityMap,
    /// Update context entity map (`PATCH /entityMaps/{id}`).
    #[serde(rename = "updateEntityMap")]
    UpdateEntityMap,
    /// Delete context entity map (`DELETE /entityMaps/{id}`).
    #[serde(rename = "deleteEntityMap")]
    DeleteEntityMap,
    /// Create entity map query on entities.
    #[serde(rename = "createEntityMapQueryEntity")]
    CreateEntityMapQueryEntity,
    /// Create entity map query on temporal entities.
    #[serde(rename = "createEntityMapQueryTemporal")]
    CreateEntityMapQueryTemporal,
    /// Retrieve context source identity (`GET /csourceIdentity`).
    #[serde(rename = "retrieveContextSourceIdentity")]
    RetrieveContextSourceIdentity,
}

impl Operation {
    /// Returns `true` if this operation modifies context or subscription state (GW15, GW16).
    pub fn is_write(&self) -> bool {
        matches!(
            self,
            Self::CreateEntity
                | Self::UpdateEntity
                | Self::AppendAttrs
                | Self::UpdateAttrs
                | Self::DeleteAttrs
                | Self::DeleteEntity
                | Self::CreateBatch
                | Self::UpsertBatch
                | Self::UpdateBatch
                | Self::DeleteBatch
                | Self::UpsertTemporal
                | Self::AppendAttrsTemporal
                | Self::DeleteAttrsTemporal
                | Self::UpdateAttrInstanceTemporal
                | Self::DeleteAttrInstanceTemporal
                | Self::DeleteTemporal
                | Self::MergeEntity
                | Self::ReplaceEntity
                | Self::ReplaceAttrs
                | Self::MergeBatch
                | Self::PurgeEntity
                | Self::CreateSubscription
                | Self::UpdateSubscription
                | Self::DeleteSubscription
        )
    }

    /// Returns the exact CIM 009 Table 4.20-1 wire name.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::CreateEntity => "createEntity",
            Self::UpdateEntity => "updateEntity",
            Self::AppendAttrs => "appendAttrs",
            Self::UpdateAttrs => "updateAttrs",
            Self::DeleteAttrs => "deleteAttrs",
            Self::DeleteEntity => "deleteEntity",
            Self::CreateBatch => "createBatch",
            Self::UpsertBatch => "upsertBatch",
            Self::UpdateBatch => "updateBatch",
            Self::DeleteBatch => "deleteBatch",
            Self::UpsertTemporal => "upsertTemporal",
            Self::AppendAttrsTemporal => "appendAttrsTemporal",
            Self::DeleteAttrsTemporal => "deleteAttrsTemporal",
            Self::UpdateAttrInstanceTemporal => "updateAttrInstanceTemporal",
            Self::DeleteAttrInstanceTemporal => "deleteAttrInstanceTemporal",
            Self::DeleteTemporal => "deleteTemporal",
            Self::MergeEntity => "mergeEntity",
            Self::ReplaceEntity => "replaceEntity",
            Self::ReplaceAttrs => "replaceAttrs",
            Self::MergeBatch => "mergeBatch",
            Self::PurgeEntity => "purgeEntity",
            Self::RetrieveEntity => "retrieveEntity",
            Self::QueryEntity => "queryEntity",
            Self::QueryBatch => "queryBatch",
            Self::RetrieveTemporal => "retrieveTemporal",
            Self::QueryTemporal => "queryTemporal",
            Self::RetrieveEntityTypes => "retrieveEntityTypes",
            Self::RetrieveEntityTypeDetails => "retrieveEntityTypeDetails",
            Self::RetrieveEntityTypeInfo => "retrieveEntityTypeInfo",
            Self::RetrieveAttrTypes => "retrieveAttrTypes",
            Self::RetrieveAttrTypeDetails => "retrieveAttrTypeDetails",
            Self::RetrieveAttrTypeInfo => "retrieveAttrTypeInfo",
            Self::CreateSubscription => "createSubscription",
            Self::UpdateSubscription => "updateSubscription",
            Self::RetrieveSubscription => "retrieveSubscription",
            Self::QuerySubscription => "querySubscription",
            Self::DeleteSubscription => "deleteSubscription",
            Self::RetrieveEntityMap => "retrieveEntityMap",
            Self::UpdateEntityMap => "updateEntityMap",
            Self::DeleteEntityMap => "deleteEntityMap",
            Self::CreateEntityMapQueryEntity => "createEntityMapQueryEntity",
            Self::CreateEntityMapQueryTemporal => "createEntityMapQueryTemporal",
            Self::RetrieveContextSourceIdentity => "retrieveContextSourceIdentity",
        }
    }

    /// All 43 operations in Table 4.20-1 order.
    pub const ALL: &'static [Operation] = &[
        Self::CreateEntity,
        Self::UpdateEntity,
        Self::AppendAttrs,
        Self::UpdateAttrs,
        Self::DeleteAttrs,
        Self::DeleteEntity,
        Self::CreateBatch,
        Self::UpsertBatch,
        Self::UpdateBatch,
        Self::DeleteBatch,
        Self::UpsertTemporal,
        Self::AppendAttrsTemporal,
        Self::DeleteAttrsTemporal,
        Self::UpdateAttrInstanceTemporal,
        Self::DeleteAttrInstanceTemporal,
        Self::DeleteTemporal,
        Self::MergeEntity,
        Self::ReplaceEntity,
        Self::ReplaceAttrs,
        Self::MergeBatch,
        Self::PurgeEntity,
        Self::RetrieveEntity,
        Self::QueryEntity,
        Self::QueryBatch,
        Self::RetrieveTemporal,
        Self::QueryTemporal,
        Self::RetrieveEntityTypes,
        Self::RetrieveEntityTypeDetails,
        Self::RetrieveEntityTypeInfo,
        Self::RetrieveAttrTypes,
        Self::RetrieveAttrTypeDetails,
        Self::RetrieveAttrTypeInfo,
        Self::CreateSubscription,
        Self::UpdateSubscription,
        Self::RetrieveSubscription,
        Self::QuerySubscription,
        Self::DeleteSubscription,
        Self::RetrieveEntityMap,
        Self::UpdateEntityMap,
        Self::DeleteEntityMap,
        Self::CreateEntityMapQueryEntity,
        Self::CreateEntityMapQueryTemporal,
        Self::RetrieveContextSourceIdentity,
    ];
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Table 4.20-2: the five distributed operation group names (R8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum OperationGroup {
    /// Consumption, subscription, and entity map operations.
    #[serde(rename = "federationOps")]
    FederationOps,
    /// Association operations without entity map support.
    #[serde(rename = "associationOps")]
    AssociationOps,
    /// Entity and attribute update operations.
    #[serde(rename = "updateOps")]
    UpdateOps,
    /// Retrieve and query entity operations.
    #[serde(rename = "retrieveOps")]
    RetrieveOps,
    /// Full redirection operations.
    #[serde(rename = "redirectionOps")]
    RedirectionOps,
}

impl OperationGroup {
    /// The operations this group stands for, CIM 009 Table 4.20-2 verbatim (R8).
    ///
    /// A grant naming a group grants exactly these; the list is the vocabulary's, not a
    /// convenience shorthand, so it is never widened locally.
    pub const fn operations(&self) -> &'static [Operation] {
        /// The consumption and subscription operations plus the EntityMap support ones.
        const FEDERATION: &[Operation] = &[
            Operation::RetrieveEntity,
            Operation::QueryEntity,
            Operation::QueryBatch,
            Operation::RetrieveEntityTypes,
            Operation::RetrieveEntityTypeDetails,
            Operation::RetrieveEntityTypeInfo,
            Operation::RetrieveAttrTypes,
            Operation::RetrieveAttrTypeDetails,
            Operation::RetrieveAttrTypeInfo,
            Operation::CreateSubscription,
            Operation::UpdateSubscription,
            Operation::RetrieveSubscription,
            Operation::QuerySubscription,
            Operation::DeleteSubscription,
            Operation::RetrieveEntityMap,
            Operation::UpdateEntityMap,
            Operation::DeleteEntityMap,
            Operation::CreateEntityMapQueryEntity,
        ];
        /// `federationOps` without the EntityMap support operations.
        const ASSOCIATION: &[Operation] = &[
            Operation::RetrieveEntity,
            Operation::QueryEntity,
            Operation::QueryBatch,
            Operation::RetrieveEntityTypes,
            Operation::RetrieveEntityTypeDetails,
            Operation::RetrieveEntityTypeInfo,
            Operation::RetrieveAttrTypes,
            Operation::RetrieveAttrTypeDetails,
            Operation::RetrieveAttrTypeInfo,
            Operation::CreateSubscription,
            Operation::UpdateSubscription,
            Operation::RetrieveSubscription,
            Operation::QuerySubscription,
            Operation::DeleteSubscription,
        ];
        const UPDATE: &[Operation] = &[
            Operation::UpdateEntity,
            Operation::UpdateAttrs,
            Operation::ReplaceEntity,
            Operation::ReplaceAttrs,
        ];
        const RETRIEVE: &[Operation] = &[Operation::RetrieveEntity, Operation::QueryEntity];
        const REDIRECTION: &[Operation] = &[
            Operation::CreateEntity,
            Operation::UpdateEntity,
            Operation::AppendAttrs,
            Operation::UpdateAttrs,
            Operation::DeleteAttrs,
            Operation::DeleteEntity,
            Operation::MergeEntity,
            Operation::ReplaceEntity,
            Operation::ReplaceAttrs,
            Operation::RetrieveEntity,
            Operation::QueryEntity,
            Operation::PurgeEntity,
            Operation::RetrieveEntityTypes,
            Operation::RetrieveEntityTypeDetails,
            Operation::RetrieveEntityTypeInfo,
            Operation::RetrieveAttrTypes,
            Operation::RetrieveAttrTypeDetails,
            Operation::RetrieveAttrTypeInfo,
            Operation::RetrieveEntityMap,
            Operation::UpdateEntityMap,
            Operation::DeleteEntityMap,
            Operation::CreateEntityMapQueryEntity,
        ];

        match self {
            Self::FederationOps => FEDERATION,
            Self::AssociationOps => ASSOCIATION,
            Self::UpdateOps => UPDATE,
            Self::RetrieveOps => RETRIEVE,
            Self::RedirectionOps => REDIRECTION,
        }
    }

    /// Whether the group stands for at least one operation that changes context data (R8).
    ///
    /// `updateOps` is the update family; `redirectionOps` forwards every operation, writes
    /// included. The consumption groups do not write.
    pub const fn includes_write(&self) -> bool {
        matches!(self, Self::UpdateOps | Self::RedirectionOps)
    }

    /// Returns the exact CIM 009 Table 4.20-2 wire name.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::FederationOps => "federationOps",
            Self::AssociationOps => "associationOps",
            Self::UpdateOps => "updateOps",
            Self::RetrieveOps => "retrieveOps",
            Self::RedirectionOps => "redirectionOps",
        }
    }

    /// All five operation groups in Table 4.20-2 order.
    pub const ALL: &'static [OperationGroup] = &[
        Self::FederationOps,
        Self::AssociationOps,
        Self::UpdateOps,
        Self::RetrieveOps,
        Self::RedirectionOps,
    ];
}

impl fmt::Display for OperationGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Reference to a CIM 009 operation or operation group (R8).
///
/// Closed enum ensures no proprietary verbs (`read`, `write`, `upsertEntity`) are permitted,
/// satisfying the strict R8 CIM 009 vocabulary guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum OperationRef {
    /// Individual named operation from CIM 009 Table 4.20-1.
    Single(Operation),
    /// Operation group from CIM 009 Table 4.20-2.
    Group(OperationGroup),
}

impl OperationRef {
    /// Returns the wire name for this operation or group.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Single(op) => op.as_str(),
            Self::Group(grp) => grp.as_str(),
        }
    }

    /// Whether granting this reference lets the holder change context data (R8, AP-09).
    ///
    /// A group counts as a write when any operation it stands for writes; erring towards
    /// "write" only raises the review lane, never lowers it.
    pub fn is_write(&self) -> bool {
        match self {
            Self::Single(op) => op.is_write(),
            Self::Group(grp) => grp.includes_write(),
        }
    }
}

impl fmt::Display for OperationRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Identity principal receiving a policy grant (ADR 002).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Principal {
    /// Category of principal.
    pub kind: PrincipalKind,
    /// Identifier of the principal (username, role name, group name, DID).
    pub id: String,
}

impl Principal {
    /// Creates a new principal with kind and identifier.
    pub fn new(kind: PrincipalKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }
}

/// Principal category for authorization rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PrincipalKind {
    /// Individual human user identity.
    User,
    /// Group of users.
    Group,
    /// Platform role.
    Role,
    /// Non-human service account.
    ServiceAccount,
    /// Decentralized identifier (`did:web:...`).
    Did,
}

impl PrincipalKind {
    /// Returns the camelCase wire name for this principal kind.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Group => "group",
            Self::Role => "role",
            Self::ServiceAccount => "serviceAccount",
            Self::Did => "did",
        }
    }
}

impl fmt::Display for PrincipalKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Entity type and id/pattern selector within a policy grant (R6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EntitySelector {
    /// Target entity type short name in PascalCase.
    #[serde(rename = "type")]
    pub entity_type: String,
    /// Explicit target entity URN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Urn>,
    /// Anchored regular expression matching target entity URNs (R24, R33).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_pattern: Option<String>,
}

/// Whether a policy grants access or takes it away (GW4, GW8).
///
/// A prohibition subtracts from a grant that stays in force. It is not how a resource is
/// kept private in the first place: the default verdict is already DENY (GW5), so a type
/// nobody was granted is closed without any policy naming it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PolicyEffect {
    /// The policy grants the operations it lists (the default).
    #[default]
    Permission,
    /// The policy denies them, and is evaluated before any permission (GW4).
    Prohibition,
}

impl PolicyEffect {
    /// Whether this policy takes access away rather than granting it.
    pub const fn is_prohibition(self) -> bool {
        matches!(self, Self::Prohibition)
    }
}

/// Specification of target entity types, properties, and relationships (R6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RegistrationInfo {
    /// Target entity selectors.
    pub entities: Vec<EntitySelector>,
    /// Whitelist of readable or writable property names (R6, R9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub property_names: Vec<String>,
    /// Whitelist of readable or writable relationship names (R6, R9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relationship_names: Vec<String>,
}

/// Temporal validity window for a policy grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Validity {
    /// Effective start timestamp (UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<DateTime<Utc>>,
    /// Effective expiration timestamp (UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<DateTime<Utc>>,
}

/// Desired specification of a [`Policy`][crate::kinds::Policy] resource (ADR 002, R1..R9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PolicySpec {
    /// Reference to the target ContextSpace.
    pub context_space_ref: Ref,
    /// Whether this policy grants or takes away (GW4, GW8).
    #[serde(default)]
    pub effect: PolicyEffect,
    /// Granting data owner (e.g. `did:web:banskabystrica.sk`).
    pub assigner: String,
    /// Grantee principal receiving permissions.
    pub assignee: Principal,
    /// Granted CIM 009 operations or operation groups (R8).
    pub operations: Vec<OperationRef>,
    /// Target entity types and attribute whitelist (R6, R9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub information: Vec<RegistrationInfo>,
    /// Residual NGSI-LD query filter string (R7, EP-56).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// Residual scope query filter string (R7, R13, EP-56).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_q: Option<String>,
    /// Residual geographic query filter string (R7, GW11, EP-56).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geo_q: Option<String>,
    /// Residual temporal query filter string (R7, GW11, EP-56).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_q: Option<String>,
    /// Optional time validity window for this policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validity: Option<Validity>,
}

impl Kind for PolicySpec {
    const KIND: &'static str = "Policy";
    const PLURAL: &'static str = "policies";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/spaces/{space}/policies/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }

    fn context_space(&self) -> Option<&str> {
        Some(self.context_space_ref.name())
    }
}

impl PolicySpec {
    /// Validates assigner, assignee, operations, entity selectors, and validity constraints.
    pub fn validate(&self) -> Result<()> {
        let space_name = self.context_space_ref.name();
        names::validate_space_name(space_name)?;
        if let Some(kind) = self.context_space_ref.kind() {
            if kind != "ContextSpace" {
                return Err(Error::Kind {
                    expected: "ContextSpace",
                    got: kind.to_string(),
                });
            }
        }

        if self.assigner.trim().is_empty() {
            return Err(Error::Name {
                field: "spec.assigner",
                value: self.assigner.clone(),
                reason: "assigner must not be empty",
            });
        }

        if self.assignee.id.trim().is_empty() {
            return Err(Error::Name {
                field: "spec.assignee.id",
                value: self.assignee.id.clone(),
                reason: "assignee id must not be empty",
            });
        }

        if self.operations.is_empty() {
            return Err(Error::Name {
                field: "spec.operations",
                value: String::new(),
                reason: "operations must not be empty",
            });
        }

        let mut seen_ops = std::collections::BTreeSet::new();
        for op in &self.operations {
            let wire = op.as_str();
            if !seen_ops.insert(wire) {
                return Err(Error::Name {
                    field: "spec.operations",
                    value: wire.to_string(),
                    reason: "duplicate operation in operations list",
                });
            }
        }

        for info in &self.information {
            for entity in &info.entities {
                names::validate_entity_type(&entity.entity_type)?;

                if entity.id.is_some() && entity.id_pattern.is_some() {
                    return Err(Error::Name {
                        field: "entities.idPattern",
                        value: entity.id_pattern.clone().unwrap_or_default(),
                        reason: "id and idPattern are mutually exclusive on an entity selector",
                    });
                }

                if let Some(ref pat) = entity.id_pattern {
                    if !pat.starts_with('^') || !pat.ends_with('$') {
                        return Err(Error::Name {
                            field: "entities.idPattern",
                            value: pat.clone(),
                            reason: "idPattern must be anchored with `^` and `$` (R24, R33)",
                        });
                    }
                    if regex::Regex::new(pat).is_err() {
                        return Err(Error::Name {
                            field: "entities.idPattern",
                            value: pat.clone(),
                            reason: "idPattern failed to compile as a valid regex",
                        });
                    }
                }
            }

            let mut seen_props = std::collections::BTreeSet::new();
            for prop in &info.property_names {
                if prop.trim().is_empty() {
                    return Err(Error::Name {
                        field: "information.propertyNames",
                        value: prop.clone(),
                        reason: "propertyName must not be empty",
                    });
                }
                if !seen_props.insert(prop.as_str()) {
                    return Err(Error::Name {
                        field: "information.propertyNames",
                        value: prop.clone(),
                        reason: "duplicate propertyName in propertyNames list",
                    });
                }
            }

            let mut seen_rels = std::collections::BTreeSet::new();
            for rel in &info.relationship_names {
                if rel.trim().is_empty() {
                    return Err(Error::Name {
                        field: "information.relationshipNames",
                        value: rel.clone(),
                        reason: "relationshipName must not be empty",
                    });
                }
                if !seen_rels.insert(rel.as_str()) {
                    return Err(Error::Name {
                        field: "information.relationshipNames",
                        value: rel.clone(),
                        reason: "duplicate relationshipName in relationshipNames list",
                    });
                }
            }
        }

        if let Some(ref val) = self.validity {
            if let (Some(from), Some(to)) = (val.from, val.to) {
                if from > to {
                    return Err(Error::Name {
                        field: "spec.validity",
                        value: format!("{from} > {to}"),
                        reason: "validity `from` must be less than or equal to `to`",
                    });
                }
            }
        }

        Ok(())
    }

    /// Returns `true` if any granted operation or group modifies context state.
    pub fn has_write(&self) -> bool {
        self.operations.iter().any(|op| match op {
            OperationRef::Single(s) => s.is_write(),
            OperationRef::Group(g) => {
                matches!(g, OperationGroup::UpdateOps | OperationGroup::FederationOps)
            }
        })
    }

    /// Returns `true` if all residual constraints (`q`, `scopeQ`, `geoQ`, `temporalQ`) are absent (EP-56).
    pub fn residual_is_empty(&self) -> bool {
        self.q.is_none()
            && self.scope_q.is_none()
            && self.geo_q.is_none()
            && self.temporal_q.is_none()
    }
}

/// Desired specification of a [`ScopeDefinition`][crate::kinds::ScopeDefinition] resource (ADR 005, R19, R29).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScopeDefinitionSpec {
    /// Multidimensional hierarchical scope path (e.g. `/geo/SK/BB/Radvan`).
    pub scope_string: String,
    /// Whether this definition represents a taxonomy root (`/geo`, `/domain`, `/admin`).
    #[serde(default)]
    pub is_root: bool,
    /// Parent scope path (absent if and only if `is_root` is true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_child_of: Option<String>,
}

impl Kind for ScopeDefinitionSpec {
    const KIND: &'static str = "ScopeDefinition";
    const PLURAL: &'static str = "policies";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/policies/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl ScopeDefinitionSpec {
    /// Validates scope string taxonomy root, segment charset, and hierarchy agreement (ADR 004, ADR 005).
    pub fn validate(&self) -> Result<()> {
        if !self.scope_string.starts_with('/') {
            return Err(Error::Name {
                field: "spec.scopeString",
                value: self.scope_string.clone(),
                reason: "scopeString must start with `/`",
            });
        }
        if self.scope_string.ends_with('/') {
            return Err(Error::Name {
                field: "spec.scopeString",
                value: self.scope_string.clone(),
                reason: "scopeString must not end with a trailing `/`",
            });
        }
        if self.scope_string.contains("//") {
            return Err(Error::Name {
                field: "spec.scopeString",
                value: self.scope_string.clone(),
                reason: "scopeString must not contain empty segments (`//`)",
            });
        }

        let segments: Vec<&str> = self.scope_string[1..].split('/').collect();
        if segments.is_empty() || !matches!(segments[0], "geo" | "domain" | "admin") {
            return Err(Error::Name {
                field: "spec.scopeString",
                value: self.scope_string.clone(),
                reason: "scopeString must start with one of the three taxonomy roots: `/geo`, `/domain`, `/admin`",
            });
        }

        for seg in &segments {
            if seg.is_empty()
                || !seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
            {
                return Err(Error::Name {
                    field: "spec.scopeString",
                    value: self.scope_string.clone(),
                    reason: "each segment must match ^[A-Za-z0-9._-]+$ and not be empty",
                });
            }
        }

        let is_root_path = segments.len() == 1;
        if self.is_root && !is_root_path {
            return Err(Error::Name {
                field: "spec.isRoot",
                value: "true".to_string(),
                reason: "isRoot can only be true for taxonomy roots (/geo, /domain, /admin)",
            });
        }
        if !self.is_root && is_root_path {
            return Err(Error::Name {
                field: "spec.isRoot",
                value: "false".to_string(),
                reason: "isRoot must be true for taxonomy roots (/geo, /domain, /admin)",
            });
        }

        if self.is_root {
            if let Some(ref parent) = self.is_child_of {
                return Err(Error::Name {
                    field: "spec.isChildOf",
                    value: parent.clone(),
                    reason: "isChildOf must be absent when isRoot is true",
                });
            }
        } else {
            let expected_parent = match self.scope_string.rfind('/') {
                Some(idx) => &self.scope_string[..idx],
                None => "",
            };
            match &self.is_child_of {
                None => {
                    return Err(Error::Name {
                        field: "spec.isChildOf",
                        value: String::new(),
                        reason: "isChildOf must be present when isRoot is false",
                    });
                }
                Some(parent) => {
                    if parent != expected_parent {
                        return Err(Error::Name {
                            field: "spec.isChildOf",
                            value: parent.clone(),
                            reason:
                                "isChildOf must match the immediate parent prefix of scopeString",
                        });
                    }
                }
            }
        }

        Ok(())
    }
}
