//! Shared name and identifier validators across joinedcontext platform kinds.

use crate::error::{Error, Result};
use regex::Regex;
use std::sync::LazyLock;

static DNS1123_LABEL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?$").expect("valid regex"));
static PROJECT_SLUG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9-]{0,62}$").expect("valid regex"));
static SPACE_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9-]{0,62}$").expect("valid regex"));
static DNS_LABEL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$").expect("valid regex"));
static LOCAL_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._~-]{1,128}$").expect("valid regex"));
static ENTITY_TYPE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z][A-Za-z0-9]{1,63}$").expect("valid regex"));
static LOCALE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z]{2}$").expect("valid regex"));

/// Validates a DNS-1123 label for metadata names (`^[a-z0-9]([-a-z0-9]*[a-z0-9])?$`, 1..=63 characters) (MF-02).
pub fn validate_dns1123_label(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 63 || !DNS1123_LABEL_RE.is_match(name) {
        return Err(Error::Name {
            field: "metadata.name",
            value: name.to_string(),
            reason: "must match DNS-1123 label regex ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ and be 1 to 63 characters",
        });
    }
    Ok(())
}

/// Validates a target namespace: literal `org` or project slug (`^[a-z0-9][a-z0-9-]{0,62}$`) (MF-02).
pub fn validate_namespace(ns: &str) -> Result<()> {
    if ns.is_empty() || ns.len() > 63 || !PROJECT_SLUG_RE.is_match(ns) {
        return Err(Error::Name {
            field: "metadata.namespace",
            value: ns.to_string(),
            reason: "must be `org` or match ^[a-z0-9][a-z0-9-]{0,62}$ and be 1 to 63 characters",
        });
    }
    Ok(())
}

/// Validates a Context Space name (`^[a-z0-9][a-z0-9-]{0,62}$`) (PF-09).
pub fn validate_space_name(space: &str) -> Result<()> {
    if space.is_empty() || space.len() > 63 || !SPACE_NAME_RE.is_match(space) {
        return Err(Error::Name {
            field: "space",
            value: space.to_string(),
            reason: "must match ^[a-z0-9][a-z0-9-]{0,62}$ and be 1 to 63 characters",
        });
    }
    Ok(())
}

/// Validates an organization's verified domain name (PF-41, PF-42).
pub fn validate_org_domain(domain: &str) -> Result<()> {
    if domain.is_empty()
        || domain.len() > 253
        || domain.starts_with('.')
        || domain.ends_with('.')
        || domain.contains(':')
        || domain.contains('/')
    {
        return Err(Error::Name {
            field: "orgDomain",
            value: domain.to_string(),
            reason: "must be a lowercase DNS name with at least two labels and total length <= 253",
        });
    }

    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return Err(Error::Name {
            field: "orgDomain",
            value: domain.to_string(),
            reason: "must contain at least two labels separated by dots",
        });
    }

    for label in labels {
        if label.is_empty() || label.len() > 63 || !DNS_LABEL_RE.is_match(label) {
            return Err(Error::Name {
                field: "orgDomain",
                value: domain.to_string(),
                reason: "each label must match ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ and be 1 to 63 characters",
            });
        }
    }

    Ok(())
}

/// Validates a local identifier within an entity URN (`^[A-Za-z0-9._~-]{1,128}$`) (PF-42).
pub fn validate_local_id(local_id: &str) -> Result<()> {
    if local_id.is_empty() || local_id.len() > 128 || !LOCAL_ID_RE.is_match(local_id) {
        return Err(Error::Name {
            field: "localId",
            value: local_id.to_string(),
            reason: "must match ^[A-Za-z0-9._~-]{1,128}$ and be 1 to 128 characters",
        });
    }
    Ok(())
}

/// Validates an entity type short name in PascalCase (`^[A-Z][A-Za-z0-9]{1,63}$`) (PF-42).
pub fn validate_entity_type(t: &str) -> Result<()> {
    if t.len() < 2 || t.len() > 64 || !ENTITY_TYPE_RE.is_match(t) {
        return Err(Error::Name {
            field: "entityType",
            value: t.to_string(),
            reason:
                "must match PascalCase regex ^[A-Z][A-Za-z0-9]{1,63}$ and be 2 to 64 characters",
        });
    }
    Ok(())
}

/// Validates an ISO 639-1 two-letter lowercase language code (`^[a-z]{2}$`) (PF-25).
pub fn validate_locale(l: &str) -> Result<()> {
    if l.len() != 2 || !LOCALE_RE.is_match(l) {
        return Err(Error::Locale(l.to_string()));
    }
    Ok(())
}

/// The prefixes a minted credential wears, lowercased, with the separator both spellings use.
///
/// Only shapes a token announces itself by: a rule that guessed from length or entropy would
/// refuse real secret names, and a name nobody can use is a worse failure than the one this
/// catches. The list is the tokens a person has in their clipboard while they configure a source.
const CREDENTIAL_PREFIXES: &[&str] = &[
    "ghp_",
    "ghp-",
    "gho_",
    "gho-",
    "ghu_",
    "ghu-",
    "ghs_",
    "ghs-",
    "ghr_",
    "ghr-",
    "github_pat_",
    "github-pat-",
    "glpat_",
    "glpat-",
    "gldt_",
    "gldt-",
    "glrt_",
    "glrt-",
    "xoxb_",
    "xoxb-",
    "xoxp_",
    "xoxp-",
    "xoxa_",
    "xoxa-",
    "xapp_",
    "xapp-",
    "sk_",
    "sk-",
    "pk_live_",
    "pk-live-",
    "sk_live_",
    "sk-live-",
    "rk_live_",
    "rk-live-",
    "dckr_pat_",
    "dckr-pat-",
    "npm_",
    "npm-",
    "hf_",
    "hf-",
    "sbp_",
    "sbp-",
    "akia",
    "asia",
    "aiza",
    "ya29.",
];

/// Whether a value carries the shape of a minted credential rather than the name of one (MF-24).
///
/// Used where a person types the *name* of a secret: a name is free text, and the mistake this
/// catches is pasting the credential itself into it, which would commit a live token to Git. A
/// token whose prefix is also a valid DNS-1123 label (`glpat-…`, `xoxb-…`, `sk-…`) passes every
/// other rule the platform has, which is why this one exists.
pub fn looks_like_a_credential(value: &str) -> bool {
    let lowered = value.to_ascii_lowercase();
    if CREDENTIAL_PREFIXES
        .iter()
        .any(|prefix| lowered.starts_with(prefix))
    {
        return true;
    }
    // A PEM block or a JWT: three dot-separated base64url segments starting with a JSON header.
    lowered.starts_with("-----begin") || (value.starts_with("eyJ") && value.split('.').count() == 3)
}
