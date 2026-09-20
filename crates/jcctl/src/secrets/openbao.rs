//! Resolving `secretRef`s from OpenBao, the Vault-compatible store (CC-06, OPS-37, PL-15,
//! ADR-N-012).
//!
//! Small installations keep their values in SOPS-encrypted files in the repository
//! ([`super::sops`]); regional ones keep them in OpenBao. A manifest cannot tell the
//! difference: it carries a `secretRef` and nothing else, and the reconciler resolves it at
//! apply time into a value that only [`SecretValue::expose`] can read.
//!
//! Two credentials are involved and neither is ever written down. The pod's projected
//! ServiceAccount JWT is exchanged for an OpenBao token at the Kubernetes auth mount, and a
//! login that comes back without an expiry is refused rather than used: the point of
//! `auth/kubernetes` is a token that dies with the apply (OPS-37). The token itself lives in
//! a wiped buffer, prints as a placeholder, and appears in no error message (PL-17).
//!
//! This module opens no socket. It builds the paths and the payloads, reads the KV v2
//! envelope and refuses what does not fit; the request is carried by a [`BaoApi`]
//! implementation, the same split the CKAN publisher uses. So the whole resolution path,
//! including every refusal, is testable without a server, and the one place that could leak
//! a value over a wire is not in this crate.

use super::sops::SecretValue;
use jc_core::SecretRef;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Where a Kubernetes pod finds the projected ServiceAccount token it logs in with.
pub const SERVICE_ACCOUNT_TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// Mount path of the Kubernetes auth method, as OpenBao enables it by default.
pub const DEFAULT_AUTH_MOUNT: &str = "kubernetes";

/// Mount path of the KV v2 secrets engine, as OpenBao enables it by default.
pub const DEFAULT_KV_MOUNT: &str = "secret";

/// Which OpenBao mounts to use and which subtree of the KV engine a project may read.
///
/// The prefix is what makes the store multi-tenant: a runner's OpenBao policy grants
/// `read` on `{kv}/data/{prefix}/*` and nothing else, so a manifest that references another
/// project's secret gets a 403 from the server rather than a value (ADR-N-012 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Mount of the Kubernetes auth method.
    pub auth_mount: String,
    /// Mount of the KV v2 engine holding the values.
    pub kv_mount: String,
    /// The OpenBao role the ServiceAccount is bound to.
    pub role: String,
    /// Path prefix under the KV mount; empty for a store with no per-project subtree.
    pub prefix: String,
}

impl Settings {
    /// Settings for one role on the default mounts.
    pub fn new(role: impl Into<String>) -> Self {
        Self {
            auth_mount: DEFAULT_AUTH_MOUNT.to_owned(),
            kv_mount: DEFAULT_KV_MOUNT.to_owned(),
            role: role.into(),
            prefix: String::new(),
        }
    }

    /// Non-default mounts, for an installation that enabled the engines elsewhere.
    pub fn mounted(mut self, auth_mount: impl Into<String>, kv_mount: impl Into<String>) -> Self {
        self.auth_mount = auth_mount.into();
        self.kv_mount = kv_mount.into();
        self
    }

    /// The subtree this reconciler reads, usually the project (ADR-N-012 §3).
    pub fn under(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// `auth/{mount}/login`, where the ServiceAccount JWT is exchanged for a token.
    pub fn login_path(&self) -> String {
        format!("auth/{}/login", trim(&self.auth_mount))
    }

    /// `{kv}/data/{prefix}/{name}`: the KV v2 read path of one secret.
    ///
    /// KV v2 puts `data` between the mount and the path, which is the difference from v1 and
    /// the usual reason a working policy reads nothing.
    pub fn read_path(&self, name: &str) -> Result<String, BaoError> {
        check_name(name)?;
        let prefix = trim(&self.prefix);
        match prefix.is_empty() {
            true => Ok(format!("{}/data/{name}", trim(&self.kv_mount))),
            false => Ok(format!("{}/data/{prefix}/{name}", trim(&self.kv_mount))),
        }
    }
}

/// The two HTTP calls the resolver makes.
///
/// The implementation carries the request and holds the address and the TLS trust; this
/// crate holds the paths, the payloads and the refusals. `path` is relative to the API root
/// (`https://{host}/v1/`), the way OpenBao's own documentation writes it.
pub trait BaoApi {
    /// `POST {path}` with a JSON body and no token: the login call.
    fn post(&mut self, path: &str, body: &Value) -> Result<Value, BaoError>;

    /// `GET {path}` with the session token as `X-Vault-Token`.
    ///
    /// `Ok(None)` for a 404, which OpenBao also answers for a path the token may not read,
    /// so a missing secret and a missing grant look the same from here.
    fn get(&self, path: &str, token: &str) -> Result<Option<Value>, BaoError>;
}

/// A logged-in session: a short-lived OpenBao token and what it was granted for.
///
/// `Debug` prints a placeholder and the lease, never the token, and the buffer is wiped when
/// the session is dropped (PL-17).
pub struct Session {
    token: Zeroizing<String>,
    lease_seconds: u64,
    renewable: bool,
}

impl Session {
    /// The token, for the transport that puts it in the `X-Vault-Token` header.
    ///
    /// Named like [`SecretValue::expose`] for the same reason: every call site is a place a
    /// credential can escape, and they should be countable.
    pub fn expose(&self) -> &str {
        &self.token
    }

    /// How long the token is valid, in seconds.
    pub fn lease_seconds(&self) -> u64 {
        self.lease_seconds
    }

    /// Whether the lease can be extended rather than re-logged-in.
    pub fn renewable(&self) -> bool {
        self.renewable
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Session(redacted, lease {}s, renewable {})",
            self.lease_seconds, self.renewable
        )
    }
}

/// Everything that can go wrong talking to OpenBao. No variant carries a secret value.
#[derive(Debug, thiserror::Error)]
pub enum BaoError {
    /// The ServiceAccount token file could not be read.
    #[error("I/O error reading the ServiceAccount token at {path}: {source}")]
    Io {
        /// Path the error happened on.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The ServiceAccount token file is empty, so there is nothing to log in with.
    #[error("the ServiceAccount token at {path} is empty; the pod has no projected token")]
    EmptyServiceAccountToken {
        /// Path of the empty file.
        path: PathBuf,
    },

    /// OpenBao could not be reached at all.
    #[error("could not reach OpenBao for {path}: {message}")]
    Transport {
        /// API path the call was made against.
        path: String,
        /// Transport message; never a payload.
        message: String,
    },

    /// OpenBao answered with an error status.
    #[error("OpenBao answered {status} for {path}: {message}")]
    Api {
        /// HTTP status.
        status: u16,
        /// API path the call was made against.
        path: String,
        /// What OpenBao said in its `errors` array.
        message: String,
    },

    /// The answer parsed as JSON but is not the shape the API documents.
    #[error("OpenBao answered {path} with an unexpected shape: {reason}")]
    Malformed {
        /// API path the call was made against.
        path: String,
        /// What was missing or wrong; never a value.
        reason: String,
    },

    /// The login succeeded but the token never expires, so it is not a session (OPS-37).
    #[error("the OpenBao role `{role}` issued a token with no expiry; the Kubernetes auth role must issue short-lived tokens")]
    TokenNeverExpires {
        /// The role that was logged in with.
        role: String,
    },

    /// A reference names a secret the store does not hold, or the token may not read.
    #[error("secret `{name}` is not readable at {path}: it does not exist, or this role has no grant on it")]
    SecretNotFound {
        /// The referenced name.
        name: String,
        /// The KV v2 path that was read.
        path: String,
    },

    /// The secret exists but its current version was deleted or destroyed.
    #[error("secret `{name}` has no live version at {path}: the current version is deleted")]
    SecretDeleted {
        /// The referenced name.
        name: String,
        /// The KV v2 path that was read.
        path: String,
    },

    /// The reference names a key the secret does not carry.
    #[error("secret `{name}` has no key `{key}`")]
    KeyNotFound {
        /// The referenced name.
        name: String,
        /// The referenced key.
        key: String,
    },

    /// A multi-key secret was referenced without saying which key.
    #[error("secret `{name}` has several keys; the reference must name one")]
    KeyRequired {
        /// The referenced name.
        name: String,
    },

    /// A KV entry holds something other than a string, which cannot become a value.
    #[error("secret `{name}` key `{key}` is not a string; a secret value has to be text")]
    NotAString {
        /// The referenced name.
        name: String,
        /// The key that holds the wrong type.
        key: String,
    },

    /// A name would leave the subtree the role is scoped to.
    #[error("secret name `{name}` is not a single path segment, so it would leave the project's subtree")]
    InvalidName {
        /// The offending name.
        name: String,
    },

    /// A reference has to be injected but does not say under which variable (PL-16).
    #[error("secret `{name}` has no `envVar`; the runner needs the variable name the bento.yaml interpolates")]
    NoEnvVar {
        /// The referenced name.
        name: String,
    },
}

/// Reads the projected ServiceAccount token a pod is given.
///
/// The buffer is wiped when it is dropped and the content never reaches an error message;
/// a JWT is a bearer credential for as long as it is valid.
pub fn service_account_jwt(path: &Path) -> Result<Zeroizing<String>, BaoError> {
    let text = Zeroizing::new(
        std::fs::read_to_string(path).map_err(|source| BaoError::Io {
            path: path.to_path_buf(),
            source,
        })?,
    );
    let trimmed = Zeroizing::new(text.trim().to_owned());
    match trimmed.is_empty() {
        true => Err(BaoError::EmptyServiceAccountToken {
            path: path.to_path_buf(),
        }),
        false => Ok(trimmed),
    }
}

/// Exchanges the pod's ServiceAccount JWT for a short-lived OpenBao token (OPS-37).
pub fn login(api: &mut impl BaoApi, settings: &Settings, jwt: &str) -> Result<Session, BaoError> {
    let path = settings.login_path();
    let answer = api.post(&path, &json!({ "role": settings.role, "jwt": jwt }))?;

    let auth = answer
        .get("auth")
        .and_then(Value::as_object)
        .ok_or_else(|| BaoError::Malformed {
            path: path.clone(),
            reason: "no `auth` object in the login answer".to_owned(),
        })?;

    let token = auth
        .get("client_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| BaoError::Malformed {
            path: path.clone(),
            reason: "no `auth.client_token` in the login answer".to_owned(),
        })?;

    let lease_seconds = auth
        .get("lease_duration")
        .and_then(Value::as_u64)
        .ok_or_else(|| BaoError::Malformed {
            path: path.clone(),
            reason: "no `auth.lease_duration` in the login answer".to_owned(),
        })?;

    // A zero lease is Vault's way of saying "never expires". That is a root token in all but
    // name, and it would outlive the apply that fetched it: refuse it here rather than
    // discover it in an incident (OPS-37).
    if lease_seconds == 0 {
        return Err(BaoError::TokenNeverExpires {
            role: settings.role.clone(),
        });
    }

    Ok(Session {
        token: Zeroizing::new(token.to_owned()),
        lease_seconds,
        renewable: auth
            .get("renewable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// The values behind a set of references, read from OpenBao once each (PL-15).
#[derive(Debug, Default)]
pub struct BaoStore {
    secrets: BTreeMap<String, BTreeMap<String, SecretValue>>,
}

impl BaoStore {
    /// Reads every secret the references name, one KV v2 call per distinct name.
    ///
    /// A reference to a missing secret fails the whole fetch: half an environment is worse
    /// than none, because the runner would start and fail on the one credential it needs.
    pub fn fetch(
        api: &impl BaoApi,
        settings: &Settings,
        session: &Session,
        references: &[SecretRef],
    ) -> Result<Self, BaoError> {
        let names: BTreeSet<&str> = references.iter().map(|r| r.name.as_str()).collect();
        let mut store = Self::default();
        for name in names {
            let path = settings.read_path(name)?;
            let answer =
                api.get(&path, session.expose())?
                    .ok_or_else(|| BaoError::SecretNotFound {
                        name: name.to_owned(),
                        path: path.clone(),
                    })?;
            store
                .secrets
                .insert(name.to_owned(), keys(name, &path, &answer)?);
        }
        Ok(store)
    }

    /// Resolves what a manifest references (CC-06).
    ///
    /// A reference with no key is allowed only when the secret carries exactly one, so a
    /// second key added in OpenBao later turns an ambiguous reference into an error instead
    /// of silently changing which value a runner gets.
    pub fn resolve(&self, reference: &SecretRef) -> Result<&SecretValue, BaoError> {
        let keys = self
            .secrets
            .get(&reference.name)
            .ok_or_else(|| BaoError::SecretNotFound {
                name: reference.name.clone(),
                path: String::new(),
            })?;

        match reference.key.as_deref() {
            Some(key) => keys.get(key).ok_or_else(|| BaoError::KeyNotFound {
                name: reference.name.clone(),
                key: key.to_owned(),
            }),
            None => match keys.iter().next() {
                Some((_, value)) if keys.len() == 1 => Ok(value),
                _ => Err(BaoError::KeyRequired {
                    name: reference.name.clone(),
                }),
            },
        }
    }

    /// The secret names that were read, sorted. Names are not secret; values are.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.secrets.keys().map(String::as_str)
    }

    /// How many secrets were read.
    pub fn len(&self) -> usize {
        self.secrets.len()
    }

    /// Whether nothing was read.
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }
}

/// The environment a runner is started with (PL-15, PL-16, PL-17).
///
/// The variable name is the reference's own `envVar`, and a reference without one is
/// refused: `bento.yaml` interpolates `${MQTT_PASSWORD}` by name (PL-16), so the manifest
/// has to say which name, and a derived one would be a convention this codebase has not
/// written down. `data_source::env_var_of` derives names for a `DataSource`'s connection
/// credentials because the connection, not the author, owns those.
///
/// The result belongs in a Kubernetes Secret. It must never become a ConfigMap: a ConfigMap
/// is readable by anything that can read the namespace and is printed by `kubectl get -o
/// yaml` in full (CC-06).
pub fn environment<'a>(
    references: &[SecretRef],
    store: &'a BaoStore,
) -> Result<BTreeMap<String, &'a SecretValue>, BaoError> {
    let mut environment = BTreeMap::new();
    for reference in references {
        let variable = reference
            .env_var
            .as_deref()
            .ok_or_else(|| BaoError::NoEnvVar {
                name: reference.name.clone(),
            })?;
        environment.insert(variable.to_owned(), store.resolve(reference)?);
    }
    Ok(environment)
}

/// The `data.data` map of a KV v2 read, as `SecretValue`s.
fn keys(name: &str, path: &str, answer: &Value) -> Result<BTreeMap<String, SecretValue>, BaoError> {
    let data = answer.get("data").ok_or_else(|| BaoError::Malformed {
        path: path.to_owned(),
        reason: "no `data` object; the KV v2 read path is `{mount}/data/{name}`".to_owned(),
    })?;

    // A deleted version answers 200 with `data: null` and a `deletion_time` in the metadata,
    // which is not the same thing as a secret that was never there.
    let Some(values) = data.get("data").and_then(Value::as_object) else {
        return Err(
            match data.get("data").map(Value::is_null).unwrap_or(false) {
                true => BaoError::SecretDeleted {
                    name: name.to_owned(),
                    path: path.to_owned(),
                },
                false => BaoError::Malformed {
                    path: path.to_owned(),
                    reason: "no `data.data` object in the KV v2 answer".to_owned(),
                },
            },
        );
    };

    let mut keys = BTreeMap::new();
    for (key, value) in values {
        let text = value.as_str().ok_or_else(|| BaoError::NotAString {
            name: name.to_owned(),
            key: key.clone(),
        })?;
        keys.insert(key.clone(), SecretValue::new(text.to_owned()));
    }
    Ok(keys)
}

/// A secret name is one path segment, so a reference cannot climb out of its own subtree.
fn check_name(name: &str) -> Result<(), BaoError> {
    let sound = !name.is_empty() && !name.contains('/') && !name.starts_with('.');
    match sound {
        true => Ok(()),
        false => Err(BaoError::InvalidName {
            name: name.to_owned(),
        }),
    }
}

fn trim(segment: &str) -> &str {
    segment.trim_matches('/')
}
