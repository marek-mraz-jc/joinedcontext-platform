//! Manifest kind for a standing query: what a space watches and where it says so (T-0913,
//! CC-72, DS-16).
//!
//! `Architecture/06`'s catalogue has listed `kind: Subscription` at
//! `projects/{p}/spaces/{s}/subscriptions/` since the plane was written, and the Portal has
//! carried the row; nothing defined the type, so every loader refused the file and the
//! repository with it. This is that type: CIM 009 clause 5.2.12's subscription, with the two
//! members a platform manifest needs beside it — the space whose broker holds it, and a
//! `secretRef` for the credential the notification carries.
//!
//! Two things it refuses on purpose. A subscription that watches nothing, because a
//! subscription with no `entities` and no `watchedAttributes` matches everything the broker
//! ever stores and is a notification storm nobody asked for. And a notification endpoint with
//! a credential written into it — in the URI, or in a `receiverInfo` header — because the
//! platform calls that URL and the credential would be in Git (MF-31, CC-06).

use crate::envelope::{Kind, ObjectMeta, Ref, Scope, SecretRef};
use crate::error::{Error, Result};
use crate::names;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One selector of the entities a subscription watches (CIM 009 clause 5.2.12).
///
/// Not `policy::EntitySelector`, which requires a type: a subscription may watch one entity by
/// its URN, and a grant may not be written that loosely.
///
/// Either a type, an id, or an id pattern; the broker's own shape, so a query written against
/// the API and a subscription written in a manifest select with the same words.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WatchedEntity {
    /// The entity type, as the model names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// One entity, by its URN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Every entity whose id matches this anchored regular expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_pattern: Option<String>,
}

/// Where a notification goes and how it is addressed (CIM 009 clause 5.2.15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NotificationEndpoint {
    /// The address the platform posts to. `https` outside the cluster, `http` only for a
    /// Service name inside it.
    pub uri: String,
    /// The media type the receiver wants; the broker's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept: Option<String>,
    /// Headers the receiver needs, values only — a credential belongs in `secretRef`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub receiver_info: Vec<KeyValue>,
    /// The credential the notification carries, resolved by the reconciler at apply time and
    /// never written into the manifest (MF-31, PF-36).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
}

/// One `key`/`value` pair, as CIM 009 writes `receiverInfo` and `notifierInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KeyValue {
    /// The header name.
    pub key: String,
    /// The header value.
    pub value: String,
}

/// What is sent when the subscription fires (CIM 009 clause 5.2.14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Notification {
    /// Where it goes.
    pub endpoint: NotificationEndpoint,
    /// The attributes the notification carries; every granted one when absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<String>,
    /// `normalized`, `concise` or `keyValues`, as the specification names them; the broker's
    /// default when absent, which is why this is a string and not an enum of ours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

/// Desired specification of a `Subscription` resource (CC-72, DS-16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SubscriptionSpec {
    /// The Context Space whose broker holds the subscription. It is written into this space's
    /// tenant and no other (SP-08, SP-09).
    pub context_space_ref: Ref,
    /// What is watched. Empty means "every type", which is only allowed together with
    /// `watchedAttributes`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<WatchedEntity>,
    /// The attributes whose change fires the subscription; any attribute when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watched_attributes: Vec<String>,
    /// The condition an entity must satisfy, in the query language of CIM 009 clause 4.9.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// The area an entity must be in, as a `geoQ` (`georel`, `geometry`, `coordinates`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geo_q: Option<String>,
    /// Where the notifications go.
    pub notification: Notification,
    /// The shortest interval between two notifications, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttling: Option<u32>,
    /// When the subscription stops, if it is not open-ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether the broker delivers; a subscription written with `isActive: false` is declared
    /// and silent, which is how one is parked without deleting it.
    #[serde(default = "default_active")]
    pub is_active: bool,
    /// What a person calls it in the Portal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_name: Option<String>,
    /// Why it exists, for whoever reads the repository later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn default_active() -> bool {
    true
}

impl Kind for SubscriptionSpec {
    const KIND: &'static str = "Subscription";
    const PLURAL: &'static str = "subscriptions";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str =
        "projects/{project}/spaces/{space}/subscriptions/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }

    fn context_space(&self) -> Option<&str> {
        Some(self.context_space_ref.name())
    }
}

/// Query parameters that carry a credential often enough that one in a notification URI is a
/// leak rather than a coincidence. The same list the agent proxy refuses an egress URL by.
const CREDENTIAL_PARAMETERS: &[&str] = &[
    "access_token",
    "api_key",
    "apikey",
    "auth",
    "key",
    "password",
    "secret",
    "sig",
    "signature",
    "token",
];

/// Header names whose value is a credential, whatever it says.
const CREDENTIAL_HEADERS: &[&str] = &["authorization", "proxy-authorization", "cookie"];

impl SubscriptionSpec {
    /// Validates the selector and the endpoint (CC-72, MF-31).
    ///
    /// The expiry is not checked here, because this crate has no clock on purpose: the gateway
    /// links it and takes chrono without `clock`, so the moment is the caller's to supply —
    /// [`validate_at`](Self::validate_at) is the same check with one.
    pub fn validate(&self) -> Result<()> {
        if self.entities.is_empty() && self.watched_attributes.is_empty() {
            return Err(Error::Invalid {
                field: "spec.entities".to_string(),
                reason: "a subscription watches something: name the entities, the attributes, \
                         or both (CC-72)"
                    .to_string(),
            });
        }
        for selector in &self.entities {
            if selector.r#type.is_none() && selector.id.is_none() && selector.id_pattern.is_none() {
                return Err(Error::Invalid {
                    field: "spec.entities".to_string(),
                    reason: "an entity selector names a type, an id or an idPattern".to_string(),
                });
            }
        }
        if let Some(throttling) = self.throttling {
            if throttling == 0 {
                return Err(Error::Name {
                    field: "spec.throttling",
                    value: "0".to_string(),
                    reason: "throttling is a number of seconds; leave it out for none",
                });
            }
        }
        self.notification.endpoint.validate()
    }

    /// [`validate`](Self::validate), and the expiry against the moment the caller names.
    ///
    /// An expiry already past is a subscription that never fires, written as though it would:
    /// whoever writes it is told so, rather than the broker silently dropping it.
    pub fn validate_at(&self, now: DateTime<Utc>) -> Result<()> {
        self.validate()?;
        match self.expires_at {
            Some(expires_at) if expires_at <= now => Err(Error::Name {
                field: "spec.expiresAt",
                value: expires_at.to_rfc3339(),
                reason: "a subscription that has already expired would never fire",
            }),
            _ => Ok(()),
        }
    }

    /// The credential the notification carries, for the reconciler to resolve (PF-36).
    pub fn secret_refs(&self) -> Vec<&SecretRef> {
        self.notification.endpoint.secret_ref.iter().collect()
    }
}

impl NotificationEndpoint {
    /// Refuses an address this platform must not call and a credential written into it.
    fn validate(&self) -> Result<()> {
        // Read by hand rather than with a URL parser: jc-core is the type crate every loader
        // links, and one more dependency here is one more in every binary. The three things
        // that matter — the scheme, the userinfo and the query names — are decidable from the
        // text, and anything past them is the reconciler's problem when it calls the address.
        let Some((scheme, rest)) = self.uri.split_once("://") else {
            return Err(Error::Name {
                field: "spec.notification.endpoint.uri",
                value: self.uri.clone(),
                reason: "a notification endpoint is an absolute URL",
            });
        };
        if !matches!(scheme, "http" | "https") {
            return Err(Error::Name {
                field: "spec.notification.endpoint.uri",
                value: scheme.to_string(),
                reason: "a notification is delivered over http or https",
            });
        }
        let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        if authority.is_empty() {
            return Err(Error::Name {
                field: "spec.notification.endpoint.uri",
                value: self.uri.clone(),
                reason: "a notification endpoint names a host",
            });
        }
        if authority.contains('@') {
            return Err(Error::Name {
                field: "spec.notification.endpoint.uri",
                value: redacted(&self.uri),
                reason: "a credential in the URL is a credential in Git: name it in \
                         spec.notification.endpoint.secretRef (MF-31, CC-06)",
            });
        }
        let query = self.uri.split_once('?').map_or("", |(_, query)| query);
        for parameter in query.split('&').filter(|pair| !pair.is_empty()) {
            let name = parameter.split('=').next().unwrap_or(parameter);
            if CREDENTIAL_PARAMETERS.contains(&name.to_ascii_lowercase().as_str()) {
                return Err(Error::Name {
                    field: "spec.notification.endpoint.uri",
                    value: redacted(&self.uri),
                    reason: "a credential in a query parameter is a credential in Git: name it \
                             in spec.notification.endpoint.secretRef (MF-31, CC-06)",
                });
            }
        }
        for header in &self.receiver_info {
            if CREDENTIAL_HEADERS.contains(&header.key.to_ascii_lowercase().as_str()) {
                return Err(Error::Name {
                    field: "spec.notification.endpoint.receiverInfo",
                    value: header.key.clone(),
                    reason: "a credential header is resolved from \
                             spec.notification.endpoint.secretRef, never written here (MF-31)",
                });
            }
        }
        Ok(())
    }
}

/// The URL without whatever was written into it, so a refusal can name the field without
/// repeating the secret it refuses (PL-17).
fn redacted(uri: &str) -> String {
    match uri.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.split(['/', '?']).next().unwrap_or(rest);
            let host = host.rsplit('@').next().unwrap_or(host);
            format!("{scheme}://{host}/…")
        }
        None => "…".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(uri: &str) -> NotificationEndpoint {
        NotificationEndpoint {
            uri: uri.to_string(),
            accept: None,
            receiver_info: Vec::new(),
            secret_ref: None,
        }
    }

    #[test]
    fn a_credential_in_the_endpoint_is_refused_and_not_repeated() {
        for uri in [
            "https://alerts.example.sk/hook?token=s3cr3t-value",
            "https://user:s3cr3t-value@alerts.example.sk/hook",
        ] {
            let error = endpoint(uri).validate().expect_err(uri);
            let message = error.to_string();
            assert!(
                !message.contains("s3cr3t-value"),
                "the refusal repeated the secret: {message}"
            );
            assert!(message.contains("secretRef"), "{message}");
        }
    }

    #[test]
    fn an_endpoint_that_is_not_a_callable_url_is_refused() {
        for uri in ["file:///etc/passwd", "alerts.example.sk/hook", ""] {
            assert!(endpoint(uri).validate().is_err(), "{uri}");
        }
        endpoint("https://alerts.example.sk/hook")
            .validate()
            .expect("an ordinary https endpoint");
    }
}
