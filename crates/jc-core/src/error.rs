//! Error types and Result alias for `jc-core`.

/// Specific failure reason when constructing or parsing an entity URN.
#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone)]
pub enum UrnError {
    /// URN prefix is missing or does not match `urn:ngsi-ld:` (case-insensitive).
    #[error("missing or invalid prefix, expected `urn:ngsi-ld:`")]
    InvalidPrefix,
    /// Colon-separated segment count after prefix is not exactly 4.
    #[error("expected exactly 4 colon-separated segments, got {got}")]
    InvalidSegmentCount {
        /// Number of segments found.
        got: usize,
    },
    /// A segment at the given index is empty.
    #[error("empty segment at index {index}")]
    EmptySegment {
        /// Zero-based segment index.
        index: usize,
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
    /// Creates a new [`ProblemDetails`] with the given status code, type slug, and title.
    pub fn new(status: u16, slug: &str, title: impl Into<String>) -> Self {
        Self {
            type_uri: format!("{PROBLEM_TYPE_BASE}{slug}"),
            title: title.into(),
            status,
            detail: None,
            instance: None,
            extensions: BTreeMap::new(),
        }
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

    /// Attaches an RFC 7807 extension member.
    pub fn with_extension(mut self, key: &str, value: serde_json::Value) -> Self {
        self.extensions.insert(key.to_string(), value);
        self
    }
}

impl From<Error> for ProblemDetails {
    fn from(err: Error) -> Self {
        match err {
            Error::Urn { .. } => ProblemDetails::urn_scheme().with_detail(err.to_string()),
            Error::Name { .. }
            | Error::Invalid { .. }
            | Error::ApiVersion(..)
            | Error::Kind { .. }
            | Error::Locale(..)
            | Error::MissingFallbackLocale(..)
            | Error::Parse(..) => ProblemDetails::bad_request().with_detail(err.to_string()),
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
