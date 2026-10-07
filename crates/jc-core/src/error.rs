//! Error types and Result alias for `jc-core`.

/// Specific failure reason when constructing or parsing an entity URN.
#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone)]
pub enum UrnError {
    /// URN prefix is missing or does not match `urn:ngsi-ld:` (case-insensitive).
    #[error("missing or invalid prefix, expected `urn:ngsi-ld:`")]
    InvalidPrefix,
    /// Nothing follows the entity type: an NGSI-LD URN is `urn:ngsi-ld:{Type}:{id}` (ADR-N-041).
    #[error("nothing after the entity type: an NGSI-LD URN is `urn:ngsi-ld:{{Type}}:{{id}}`")]
    MissingId,
    /// The part after the type is not an RFC 8141 namespace-specific string (ADR-N-041, PF-43).
    #[error("invalid id part `{segment}`: {reason}")]
    InvalidId {
        /// Offending text after the type.
        segment: String,
        /// Reason for failure.
        reason: &'static str,
    },
    /// The entity type segment failed validation.
    #[error("invalid entity type segment `{segment}`: {reason}")]
    InvalidEntityType {
        /// Offending segment text.
        segment: String,
        /// Reason for failure.
        reason: &'static str,
    },
    /// The organization domain segment failed validation.
    #[error("invalid orgDomain segment `{segment}`: {reason}")]
    InvalidOrgDomain {
        /// Offending segment text.
        segment: String,
        /// Reason for failure.
        reason: &'static str,
    },
    /// The space segment failed validation.
    #[error("invalid space segment `{segment}`: {reason}")]
    InvalidSpace {
        /// Offending segment text.
        segment: String,
        /// Reason for failure.
        reason: &'static str,
    },
    /// The local identifier segment failed validation.
    #[error("invalid localId segment `{segment}`: {reason}")]
    InvalidLocalId {
        /// Offending segment text.
        segment: String,
        /// Reason for failure.
        reason: &'static str,
    },
}

/// Primary error enum for `jc-core`.
#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone)]
pub enum Error {
    /// An entity URN is invalid.
    #[error("invalid URN `{urn}`: {reason}")]
    Urn {
        /// The raw URN string.
        urn: String,
        /// Specific failure reason.
        reason: UrnError,
    },
    /// An identifier or field value failed validation.
    #[error("invalid {field} `{value}`: {reason}")]
    Name {
        /// Name of the field that failed validation.
        field: &'static str,
        /// The invalid value.
        value: String,
        /// Human-readable explanation.
        reason: &'static str,
    },
    /// A field value failed validation with dynamic details.
    #[error("invalid {field}: {reason}")]
    Invalid {
        /// Name of the field that failed validation.
        field: String,
        /// Human-readable explanation.
        reason: String,
    },
    /// The manifest apiVersion does not match joinedcontext.com/v1alpha1.
    #[error("apiVersion `{0}` is not served for this kind: every kind is `joinedcontext.com/v1alpha1`, a Pipeline may also be `joinedcontext.com/v1alpha2`")]
    ApiVersion(String),
    /// The manifest kind does not match the expected kind for the struct.
    #[error("kind must be `{expected}`, got `{got}`")]
    Kind {
        /// Expected kind name.
        expected: &'static str,
        /// Actual kind encountered.
        got: String,
    },
    /// A language code is not an ISO 639-1 two-letter code.
    #[error("locale `{0}` is not an ISO 639-1 two-letter code")]
    Locale(String),
    /// The designated fallback locale has no translation in the map.
    #[error("no value for the fallback locale `{0}`")]
    MissingFallbackLocale(String),
    /// A manifest could not be parsed at all (malformed YAML/JSON, unknown field, wrong type).
    ///
    /// Carries the serde message; untyped consumers (`jcctl validate`, the Portal import
    /// wizard) report it verbatim.
    #[error("manifest does not parse: {0}")]
    Parse(String),
}

/// Result type alias for operations in `jc-core`.
pub type Result<T> = std::result::Result<T, Error>;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The media type every error response carries (RFC 7807).
pub const PROBLEM_JSON: &str = "application/problem+json";
/// Base IRI of the platform's problem types.
pub const PROBLEM_TYPE_BASE: &str = "https://joinedcontext.com/errors/";

/// One problem type of the platform: its slug, the status it answers with, its title and what the
/// caller does about it (T-3243, API/00 §4). Every `type` the platform writes is one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProblemType {
    /// The last segment of the type IRI.
    pub slug: &'static str,
    /// The status it answers with; `0` for a type that carries the status of what failed.
    pub status: u16,
    /// The RFC 7807 title.
    pub title: &'static str,
    /// What to do about it, one English sentence; the Portal translates it by the slug.
    pub hint: &'static str,
}

/// The platform's problem types (API/00 §4). A slug outside this list fails the catalogue test.
#[rustfmt::skip]
pub const PROBLEM_TYPES: &[ProblemType] = &[
    ProblemType { slug: "bad-request", status: 400, title: "Bad Request", hint: "Correct the request as the detail says and send it again." },
    ProblemType { slug: "invalid-body", status: 400, title: "Invalid Body", hint: "Send a JSON body of the documented shape; the detail names what is wrong." },
    ProblemType { slug: "urn-scheme", status: 400, title: "Entity Identifier Violates the URN Scheme", hint: "Use an entity id of the form urn:ngsi-ld:{Type}:{id}." },
    ProblemType { slug: "unauthorized", status: 401, title: "Authentication Required", hint: "Sign in again, or send a valid bearer token for this audience." },
    ProblemType { slug: "forbidden", status: 403, title: "Access Denied by Policy", hint: "Ask a project owner or an organization administrator for the role this needs." },
    ProblemType { slug: "domain-not-verified", status: 403, title: "Domain Not Verified", hint: "Verify the domain in the organization settings first." },
    ProblemType { slug: "resource-not-found", status: 404, title: "Resource Not Found", hint: "Check the name in the address; it does not exist or is not shared with you." },
    ProblemType { slug: "method-not-allowed", status: 405, title: "Method Not Allowed", hint: "Use one of the methods the Allow header lists." },
    ProblemType { slug: "read-only-view", status: 405, title: "Read-Only View", hint: "This view only reads; write through the endpoint it is built on." },
    ProblemType { slug: "conflict", status: 409, title: "Conflict", hint: "Read the resource again and repeat the change on its current state." },
    ProblemType { slug: "precondition-failed", status: 412, title: "Precondition Failed", hint: "Read the entity again and send its current version in If-Match." },
    ProblemType { slug: "payload-too-large", status: 413, title: "Payload Too Large", hint: "Send less at once: split the request or the file." },
    ProblemType { slug: "unsupported-media-type", status: 415, title: "Unsupported Media Type", hint: "Send the body as application/json or application/ld+json." },
    ProblemType { slug: "too-many-requests", status: 429, title: "Too Many Requests", hint: "Wait the seconds Retry-After names, then try again." },
    ProblemType { slug: "daily-budget", status: 429, title: "Daily Budget Spent", hint: "Today's allowance is used up; try again tomorrow or ask an administrator to raise it." },
    ProblemType { slug: "egress-budget-spent", status: 429, title: "Egress Budget Spent", hint: "This run's outbound allowance is used up; start a new run or ask an administrator to raise it." },
    ProblemType { slug: "internal-error", status: 500, title: "Internal Server Error", hint: "Try again; if it happens again, report the requestId." },
    ProblemType { slug: "subscription-not-routable", status: 501, title: "Subscription Not Routable", hint: "This deployment cannot deliver to that address; use an HTTP(S) receiver the platform reaches." },
    ProblemType { slug: "federation-identity-unavailable", status: 501, title: "Federation Identity Unavailable", hint: "This deployment has no federation identity; ask an administrator to configure one." },
    ProblemType { slug: "broker-failure", status: 0, title: "Broker Failure", hint: "The context broker failed this request; try again in a minute, and report the requestId if it repeats." },
    ProblemType { slug: "upstream-unavailable", status: 0, title: "Upstream Unavailable", hint: "A service behind the platform did not answer; try again in a minute." },
    ProblemType { slug: "upstream-redirect", status: 502, title: "Upstream Redirect", hint: "The service behind the platform answered with a redirect it may not follow; ask its owner for the final address." },
    ProblemType { slug: "service-unavailable", status: 503, title: "Service Unavailable", hint: "The platform is not ready for this yet; try again in a minute." },
    ProblemType { slug: "delivery-timeout", status: 504, title: "Delivery Timeout", hint: "The receiver did not answer in time; check that it is reachable and try again." },
];

/// The problem type of `slug`, when it is one of the platform's.
pub fn problem_type(slug: &str) -> Option<&'static ProblemType> {
    PROBLEM_TYPES.iter().find(|known| known.slug == slug)
}

/// RFC 7807 Problem Details representation for HTTP error responses.
///
/// **Security note**:
/// - `internal()` MUST NOT be given caller-derived or internal error details to prevent information leakage (R5).
///   Use [`ProblemDetails::internal_opaque`] to record a correlation request ID in the `requestId` extension
///   while returning a safe, fixed generic detail message.
/// - `not_found()` produces a byte-identical body for both "resource does not exist" and "caller is not authorized
///   to see it", preventing existence disclosure (R20). A data-plane 404 MUST NOT call `with_detail`; only the
///   configuration API, whose caller already holds project-level access, may add diagnostic detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProblemDetails {
    /// URI reference identifying the problem type.
    #[serde(rename = "type")]
    pub type_uri: String,
    /// Short, human-readable summary of the problem type.
    pub title: String,
    /// HTTP status code generated by the origin server.
    pub status: u16,
    /// Human-readable explanation specific to this occurrence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// URI reference identifying the specific occurrence of the problem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// RFC 7807 extension members, serialized flat beside the standard ones.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl ProblemDetails {
    /// Creates a new [`ProblemDetails`] with the given status code, type slug, and title; a slug
    /// of [`PROBLEM_TYPES`] carries its `hint` (T-3243).
    pub fn new(status: u16, slug: &str, title: impl Into<String>) -> Self {
        let mut extensions = BTreeMap::new();
        if let Some(known) = problem_type(slug) {
            extensions.insert(
                "hint".to_string(),
                serde_json::Value::String(known.hint.to_string()),
            );
        }
        Self {
            type_uri: format!("{PROBLEM_TYPE_BASE}{slug}"),
            title: title.into(),
            status,
            detail: None,
            instance: None,
            extensions,
        }
    }

    /// The platform's own type for `status`: the one problem type answering with it, else the
    /// generic one of its class (`bad-request` for a 4xx, `internal-error` for a 5xx) with the
    /// status kept. A handler that only knows a status uses this, never a slug made of the
    /// status's reason phrase.
    pub fn for_status(status: u16) -> Self {
        let generic = if (400..500).contains(&status) {
            "bad-request"
        } else {
            "internal-error"
        };
        let known = match status {
            400 => "bad-request",
            401 => "unauthorized",
            403 => "forbidden",
            404 => "resource-not-found",
            405 => "method-not-allowed",
            409 => "conflict",
            412 => "precondition-failed",
            413 => "payload-too-large",
            415 => "unsupported-media-type",
            429 => "too-many-requests",
            502 => "upstream-unavailable",
            503 => "service-unavailable",
            504 => "delivery-timeout",
            _ => generic,
        };
        let title = problem_type(known).map_or("Error", |known| known.title);
        Self::new(status, known, title)
    }

    /// 404 Not Found (`resource-not-found`).
    pub fn not_found() -> Self {
        Self::new(404, "resource-not-found", "Resource Not Found")
    }

    /// 403 Forbidden (`forbidden`).
    pub fn forbidden() -> Self {
        Self::new(403, "forbidden", "Access Denied by Policy")
    }

    /// 400 Bad Request (`bad-request`).
    pub fn bad_request() -> Self {
        Self::new(400, "bad-request", "Bad Request")
    }

    /// 409 Conflict (`conflict`).
    pub fn conflict() -> Self {
        Self::new(409, "conflict", "Conflict")
    }

    /// 500 Internal Server Error (`internal-error`).
    pub fn internal() -> Self {
        Self::new(500, "internal-error", "Internal Server Error")
    }

    /// 400 Bad Request for URN scheme violation (`urn-scheme`, PF-42).
    pub fn urn_scheme() -> Self {
        Self::new(
            400,
            "urn-scheme",
            "Entity Identifier Violates the URN Scheme",
        )
    }

    /// 401 Unauthorized (`unauthorized`, PF-46).
    pub fn unauthorized() -> Self {
        Self::new(401, "unauthorized", "Authentication Required")
    }

    /// 500 Internal Server Error with safe, fixed detail and a `requestId` extension member (R5).
    pub fn internal_opaque(request_id: &str) -> Self {
        let mut pd = Self::internal().with_detail("An unexpected internal error occurred");
        pd.extensions.insert(
            "requestId".to_string(),
            serde_json::Value::String(request_id.to_string()),
        );
        pd
    }

    /// Attaches an occurrence-specific explanation.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Attaches an instance URI identifying the occurrence.
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Names the request member or parameter a validation refused (T-3243).
    pub fn with_field(self, field: impl Into<String>) -> Self {
        self.with_extension("field", serde_json::Value::String(field.into()))
    }

    /// Attaches an RFC 7807 extension member.
    pub fn with_extension(mut self, key: &str, value: serde_json::Value) -> Self {
        self.extensions.insert(key.to_string(), value);
        self
    }
}

impl From<Error> for ProblemDetails {
    fn from(err: Error) -> Self {
        match &err {
            Error::Urn { .. } => ProblemDetails::urn_scheme()
                .with_field("id")
                .with_detail(err.to_string()),
            Error::Name { field, .. } => {
                let field = (*field).to_string();
                ProblemDetails::bad_request()
                    .with_field(field)
                    .with_detail(err.to_string())
            }
            Error::Invalid { field, .. } => {
                let field = field.clone();
                ProblemDetails::bad_request()
                    .with_field(field)
                    .with_detail(err.to_string())
            }
            Error::ApiVersion(..) => ProblemDetails::bad_request()
                .with_field("apiVersion")
                .with_detail(err.to_string()),
            Error::Kind { .. } => ProblemDetails::bad_request()
                .with_field("kind")
                .with_detail(err.to_string()),
            Error::Locale(..) | Error::MissingFallbackLocale(..) | Error::Parse(..) => {
                ProblemDetails::bad_request().with_detail(err.to_string())
            }
        }
    }
}

#[cfg(feature = "axum")]
impl axum::response::IntoResponse for ProblemDetails {
    fn into_response(self) -> axum::response::Response {
        let status = axum::http::StatusCode::from_u16(self.status)
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let headers = [(axum::http::header::CONTENT_TYPE, PROBLEM_JSON)];
        let body = serde_json::to_string(&self).unwrap_or_else(|_| {
            format!(
                "{{\"type\":\"{PROBLEM_TYPE_BASE}internal-error\",\"title\":\"Internal Server Error\",\"status\":500}}"
            )
        });
        (status, headers, body).into_response()
    }
}
