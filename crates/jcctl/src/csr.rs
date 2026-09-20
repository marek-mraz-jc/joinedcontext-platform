//! One `ContextSourceRegistration` manifest as one broker registration (T-0303, MF-36, SP-09).
//!
//! The broker already knows what to do with a registration: CIM 009 clause 4.3.6 says how a
//! query is matched against one, forwarded, merged and loop-protected. So this module does not
//! model federation. It turns a manifest into the `csourceRegistration` the specification
//! defines, decides whether the broker's copy already says that, and calls the one action that
//! is missing.
//!
//! Like the CKAN publisher next door it opens no socket and holds no credential. The tenant
//! header of the space is added by the [`BrokerApi`] implementation on the internal hop and
//! nowhere else (SP-08, SP-09), and the identity a forward carries is a `ServiceAccount` the
//! gateway resolves at request time, never a token written into a payload (PF-48, CC-06).

use crate::loader::RawManifest;
use jc_core::kinds::{ContextSourceRegistrationSpec, FederationIdentity, RegistrationMode};
use serde_json::{json, Map, Value};

/// What one registration run did (CC-18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The broker did not know the source and now does.
    Created,
    /// The broker's copy differed from the manifest and was brought back to it.
    Updated,
    /// The broker already matched the manifest; no call was made.
    Unchanged,
    /// The registration was removed (CC-19).
    Deleted,
}

/// Why a registration could not be built or carried out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CsrError {
    /// The manifest is not a `ContextSourceRegistration`.
    #[error("not a ContextSourceRegistration manifest")]
    NotARegistration,
    /// The spec does not parse or does not validate.
    #[error("registration spec: {0}")]
    Spec(String),
    /// A registration on this platform names an Endpoint whose address the caller did not
    /// resolve. The reconciler knows the slugs; this module is given the answer.
    #[error("no address for endpointRef {0}")]
    UnresolvedEndpoint(String),
    /// The broker refused or could not be reached.
    #[error(transparent)]
    Broker(#[from] BrokerError),
}

/// Why the broker did not answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrokerError {
    /// The broker could not be reached. Carries no credential.
    #[error("broker unavailable: {0}")]
    Unavailable(String),
    /// The broker answered and refused.
    #[error("broker rejected {operation}: {message}")]
    Rejected {
        /// What was attempted.
        operation: String,
        /// What the broker said.
        message: String,
    },
}

/// The part of `/ngsi-ld/v1/csourceRegistrations` a reconciler uses.
///
/// The implementation carries the space's tenant on the request and holds whatever credential
/// the internal hop needs; nothing built here does (SP-09).
pub trait BrokerApi {
    /// The registration with this id, or `None` when the broker answers `404`.
    fn get(&self, tenant: &str, id: &str) -> Result<Option<Value>, BrokerError>;

    /// `POST /ngsi-ld/v1/csourceRegistrations`.
    fn create(&mut self, tenant: &str, body: &Value) -> Result<(), BrokerError>;

    /// `PATCH /ngsi-ld/v1/csourceRegistrations/{id}`.
    fn update(&mut self, tenant: &str, id: &str, body: &Value) -> Result<(), BrokerError>;

    /// `DELETE /ngsi-ld/v1/csourceRegistrations/{id}`.
    fn delete(&mut self, tenant: &str, id: &str) -> Result<(), BrokerError>;
}

/// A member of this platform, as the broker is told to read it (CIM 009 clause 5.2.9).
///
/// The hub's tenant reads the member's tenant directly (PF-48): the registration carries the
/// address of the broker that holds the member and the tenant its space is, and the broker
/// does the rest. Neither is decided here — the reconciler knows both and passes them in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSource {
    /// The NGSI-LD base URL of the broker holding the member.
    pub endpoint: String,
    /// The tenant the member's Context Space is, sent by the broker on every forward.
    pub tenant: String,
}

/// Where a registration's target lives, resolved by the caller.
///
/// A manifest names an `Endpoint` by name; only the reconciler holds the table that turns that
/// into a broker and a tenant, so it is passed in rather than looked up here.
pub trait Endpoints {
    /// The member behind the Endpoint with this name, or `None` when it is unknown.
    fn member_of(&self, name: &str) -> Option<MemberSource>;
}

impl<F> Endpoints for F
where
    F: Fn(&str) -> Option<MemberSource>,
{
    fn member_of(&self, name: &str) -> Option<MemberSource> {
        self(name)
    }
}

/// The NGSI-LD id a registration is addressed by.
///
/// Derived from the manifest's name rather than generated, because the reconciler has to find
/// its own previous work on a second run and a random id would leave orphans behind (CC-18).
pub fn registration_id(name: &str) -> String {
    format!("urn:ngsi-ld:ContextSourceRegistration:{name}")
}

/// The `csourceRegistration` one manifest becomes (CIM 009 clause 5.2.9).
pub fn registration(manifest: &RawManifest, endpoints: &impl Endpoints) -> Result<Value, CsrError> {
    if manifest.kind != "ContextSourceRegistration" {
        return Err(CsrError::NotARegistration);
    }
    let spec: ContextSourceRegistrationSpec =
        serde_json::from_value(manifest.spec.clone()).map_err(|e| CsrError::Spec(e.to_string()))?;
    spec.validate().map_err(|e| CsrError::Spec(e.to_string()))?;

    // A member of this platform is read as a tenant of a broker we run; a source elsewhere is
    // an address and nothing else, because its tenants are not ours to name (PF-48).
    let member = match (&spec.endpoint_ref, &spec.endpoint) {
        (Some(reference), _) => endpoints
            .member_of(reference.name())
            .ok_or_else(|| CsrError::UnresolvedEndpoint(reference.name().to_owned()))?,
        (None, Some(url)) => MemberSource {
            endpoint: url.clone(),
            tenant: String::new(),
        },
        // validate() has already refused this, so reaching it means the spec changed without
        // this match changing with it.
        (None, None) => return Err(CsrError::Spec("no target".into())),
    };

    let mut body = Map::new();
    body.insert("id".into(), json!(registration_id(&manifest.metadata.name)));
    body.insert("type".into(), json!("ContextSourceRegistration"));
    body.insert("endpoint".into(), json!(member.endpoint));
    if !member.tenant.is_empty() {
        body.insert("tenant".into(), json!(member.tenant));
    }
    body.insert("information".into(), information(&spec));
    body.insert("mode".into(), json!(mode(spec.mode)));
    if !spec.operations.is_empty() {
        body.insert("operations".into(), json!(spec.operations));
    }
    // One string, because `registrationName` is a string in the specification and a language
    // map is a shape the broker does not define. The manifest keeps every language; the Portal
    // reads the manifest.
    if let Some(title) = manifest
        .metadata
        .rest
        .get("title")
        .and_then(Value::as_object)
        .and_then(|languages| languages.values().find_map(Value::as_str))
    {
        body.insert("registrationName".into(), json!(title));
    }
    if let Some(expires) = spec.expires_at {
        // Through serde rather than chrono's own formatter: the manifest's timestamp is
        // serialised by the same code that parsed it, so the string the broker is told cannot
        // drift from the string the repository holds. jcctl does not depend on chrono.
        body.insert(
            "expiresAt".into(),
            serde_json::to_value(expires).map_err(|e| CsrError::Spec(e.to_string()))?,
        );
    }
    Ok(Value::Object(body))
}

/// The `information` array, which is the manifest's own: the manifest carries CIM 009's shape
/// so that nothing here has to translate one coverage claim into another.
fn information(spec: &ContextSourceRegistrationSpec) -> Value {
    Value::Array(
        spec.information
            .iter()
            .map(|info| {
                let mut entry = Map::new();
                entry.insert(
                    "entities".into(),
                    Value::Array(
                        info.entities
                            .iter()
                            .map(|selector| {
                                let mut e = Map::new();
                                e.insert("type".into(), json!(selector.entity_type));
                                if let Some(id) = &selector.id {
                                    e.insert("id".into(), json!(id.to_string()));
                                }
                                if let Some(pattern) = &selector.id_pattern {
                                    e.insert("idPattern".into(), json!(pattern));
                                }
                                Value::Object(e)
                            })
                            .collect(),
                    ),
                );
                if !info.property_names.is_empty() {
                    entry.insert("propertyNames".into(), json!(info.property_names));
                }
                if !info.relationship_names.is_empty() {
                    entry.insert("relationshipNames".into(), json!(info.relationship_names));
                }
                Value::Object(entry)
            })
            .collect(),
    )
}

const fn mode(mode: RegistrationMode) -> &'static str {
    match mode {
        RegistrationMode::Inclusive => "inclusive",
        RegistrationMode::Exclusive => "exclusive",
        RegistrationMode::Auxiliary => "auxiliary",
        RegistrationMode::Redirect => "redirect",
    }
}

/// Whether the forward carries the caller's own token (PF-48).
///
/// The gateway needs this at request time and the broker never sees it: a registration is where
/// the data is, and who may read it is decided by the two policy sets, not here.
pub fn forwards_caller_identity(spec: &ContextSourceRegistrationSpec) -> bool {
    spec.federation.identity == FederationIdentity::Caller
}

/// Brings the broker's copy of one registration to what the manifest says.
///
/// Idempotent: a second run over an unchanged manifest makes no writing call, so `jcctl apply`
/// converges instead of re-registering every source on every run (CC-18).
pub fn apply(
    api: &mut impl BrokerApi,
    tenant: &str,
    manifest: &RawManifest,
    endpoints: &impl Endpoints,
) -> Result<Outcome, CsrError> {
    let desired = registration(manifest, endpoints)?;
    let id = desired["id"].as_str().unwrap_or_default().to_owned();
    match api.get(tenant, &id)? {
        None => {
            api.create(tenant, &desired)?;
            Ok(Outcome::Created)
        }
        Some(live) if same(&live, &desired) => Ok(Outcome::Unchanged),
        Some(_) => {
            // The id is in the path, and a PATCH body carrying it is refused by the
            // specification's own rules on immutable members.
            let mut patch = desired.clone();
            if let Some(object) = patch.as_object_mut() {
                object.remove("id");
                object.remove("type");
            }
            api.update(tenant, &id, &patch)?;
            Ok(Outcome::Updated)
        }
    }
}

/// Removes a registration the repository no longer declares (CC-19).
///
/// Removing what is not there succeeds: an apply has to be re-runnable after a partial failure.
pub fn remove(api: &mut impl BrokerApi, tenant: &str, name: &str) -> Result<Outcome, CsrError> {
    let id = registration_id(name);
    if api.get(tenant, &id)?.is_none() {
        return Ok(Outcome::Unchanged);
    }
    api.delete(tenant, &id)?;
    Ok(Outcome::Deleted)
}

/// Whether the broker's copy already says what the manifest says.
///
/// Compares only the members this reconciler owns. The broker adds its own — `createdAt`,
/// `modifiedAt`, `@context`, a status — and treating those as drift would rewrite every
/// registration on every run (CC-69).
fn same(live: &Value, desired: &Value) -> bool {
    let Some(desired) = desired.as_object() else {
        return false;
    };
    desired.iter().all(|(key, value)| {
        if key == "id" || key == "type" {
            return true;
        }
        live.get(key) == Some(value)
    })
}

/// A broker that keeps its registrations in memory, for tests and for `jcctl plan --dry-run`.
#[derive(Debug, Default)]
pub struct InMemoryBroker {
    registrations: std::collections::BTreeMap<(String, String), Value>,
    calls: Vec<(String, String, String)>,
    last_body: Option<Value>,
}

impl InMemoryBroker {
    /// An empty broker.
    pub fn new() -> Self {
        Self::default()
    }

    /// The registration stored under one tenant, if any.
    pub fn registration(&self, tenant: &str, id: &str) -> Option<&Value> {
        self.registrations.get(&(tenant.to_owned(), id.to_owned()))
    }

    /// Every write this broker was asked to make, as `(operation, tenant, id)`.
    pub fn calls(&self) -> &[(String, String, String)] {
        &self.calls
    }

    /// Every tenant that holds a registration, in sorted order.
    pub fn tenants(&self) -> Vec<&str> {
        self.registrations.keys().map(|(t, _)| t.as_str()).collect()
    }

    /// The body of the last write, so a test can assert what a `PATCH` did and did not carry.
    pub fn last_body(&self) -> Option<&Value> {
        self.last_body.as_ref()
    }

    /// Adds the members a real broker stamps on by itself — `createdAt`, `@context`, a status.
    /// A reconciler that read those as drift would rewrite every registration on every run.
    pub fn stamp<I>(&mut self, tenant: &str, id: &str, members: I)
    where
        I: IntoIterator<Item = (&'static str, Value)>,
    {
        if let Some(stored) = self
            .registrations
            .get_mut(&(tenant.to_owned(), id.to_owned()))
            .and_then(Value::as_object_mut)
        {
            for (key, value) in members {
                stored.insert(key.to_owned(), value);
            }
        }
    }
}

impl BrokerApi for InMemoryBroker {
    fn get(&self, tenant: &str, id: &str) -> Result<Option<Value>, BrokerError> {
        Ok(self.registration(tenant, id).cloned())
    }

    fn create(&mut self, tenant: &str, body: &Value) -> Result<(), BrokerError> {
        let id = body["id"].as_str().unwrap_or_default().to_owned();
        self.calls
            .push(("create".into(), tenant.to_owned(), id.clone()));
        self.last_body = Some(body.clone());
        self.registrations
            .insert((tenant.to_owned(), id), body.clone());
        Ok(())
    }

    fn update(&mut self, tenant: &str, id: &str, body: &Value) -> Result<(), BrokerError> {
        self.calls
            .push(("update".into(), tenant.to_owned(), id.to_owned()));
        self.last_body = Some(body.clone());
        let key = (tenant.to_owned(), id.to_owned());
        let stored = self.registrations.entry(key).or_insert_with(|| json!({}));
        if let (Some(stored), Some(patch)) = (stored.as_object_mut(), body.as_object()) {
            for (k, v) in patch {
                stored.insert(k.clone(), v.clone());
            }
        }
        Ok(())
    }

    fn delete(&mut self, tenant: &str, id: &str) -> Result<(), BrokerError> {
        self.calls
            .push(("delete".into(), tenant.to_owned(), id.to_owned()));
        self.registrations
            .remove(&(tenant.to_owned(), id.to_owned()));
        Ok(())
    }
}
