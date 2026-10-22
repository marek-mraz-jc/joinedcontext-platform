//! `spec.deprecation` of an Endpoint: a breaking change announced ahead (EP-146, EP-147).
//!
//! The gateway turns it into the `Deprecation` (RFC 9745), `Sunset` (RFC 8594) and
//! `Link; rel="deprecation"` headers; the values carry the two dates and the author's public
//! link, nothing internal.

use crate::error::{Error, Result};
use crate::kinds::pipeline_lifetime::is_slot_name;
use crate::kinds::Representation;
use chrono::NaiveDate;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The least notice between `since` and `sunset` (EP-146).
pub const NOTICE_DAYS: i64 = 90;

/// The longest `link` (EP-146).
pub const MAX_LINK_LEN: usize = 2048;

/// A breaking change of a public Endpoint, announced ahead (EP-146).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Deprecation {
    /// The day the deprecation was announced, `YYYY-MM-DD`.
    pub since: NaiveDate,
    /// The earliest day the deprecated part may stop answering, at least 90 days after `since`.
    pub sunset: NaiveDate,
    /// A public `https` page about the change and its replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Only these surfaces are deprecated; empty, the whole Endpoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub representations: Vec<Representation>,
    /// Only these attributes are deprecated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<String>,
}

impl Deprecation {
    /// Checks the notice period, the link and that each surface is one the Endpoint `serves`.
    pub fn validate(&self, serves: &[Representation]) -> Result<()> {
        if self.sunset - self.since < chrono::Duration::days(NOTICE_DAYS) {
            return Err(Error::Invalid {
                field: "spec.deprecation.sunset".to_owned(),
                reason: format!(
                    "a breaking change is announced at least {NOTICE_DAYS} days ahead: the \
                     sunset is {} days after since {}; set it to {} or later (EP-146)",
                    (self.sunset - self.since).num_days(),
                    self.since,
                    self.since + chrono::Duration::days(NOTICE_DAYS),
                ),
            });
        }
        if let Some(link) = &self.link {
            if !is_public_link(link) {
                return Err(Error::Invalid {
                    field: "spec.deprecation.link".to_owned(),
                    reason: "the link is an https URL of at most 2048 characters, with a host \
                             and no user or password, e.g. https://example.org/changes (EP-146)"
                        .to_owned(),
                });
            }
        }
        if let Some(rep) = self.representations.iter().find(|r| !serves.contains(r)) {
            return Err(Error::Invalid {
                field: "spec.deprecation.representations".to_owned(),
                reason: format!(
                    "the Endpoint does not serve `{rep}`, so no consumer could be warned there \
                     (EP-146)"
                ),
            });
        }
        if let Some(name) = self.attributes.iter().find(|a| !is_slot_name(a)) {
            return Err(Error::Invalid {
                field: "spec.deprecation.attributes".to_owned(),
                reason: format!("`{name}` is not an attribute name (EP-146)"),
            });
        }
        Ok(())
    }

    /// Whether an answer of `surface` carries the headers (EP-147): a surface-scoped deprecation
    /// on its surfaces only; any other on every answer under the Endpoint's address, the record
    /// and the schema documents (`None`) included.
    pub fn covers(&self, surface: Option<Representation>) -> bool {
        self.representations.is_empty()
            || surface.is_some_and(|surface| self.representations.contains(&surface))
    }
}

/// An `https` URL with a host, no credentials, no whitespace or control character: a value that
/// goes into a `Link` header as it is.
fn is_public_link(link: &str) -> bool {
    let Some(rest) = link.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    link.len() <= MAX_LINK_LEN
        && !authority.is_empty()
        && !authority.contains('@')
        && link
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'<' | b'>' | b'"' | b'\\'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deprecation(since: &str, sunset: &str) -> Deprecation {
        Deprecation {
            since: since.parse().expect("date"),
            sunset: sunset.parse().expect("date"),
            link: None,
            representations: Vec::new(),
            attributes: Vec::new(),
        }
    }

    fn field_of(err: Error) -> String {
        match err {
            Error::Invalid { field, .. } => field,
            other => other.to_string(),
        }
    }

    #[test]
    fn ninety_days_is_enough_and_eighty_nine_is_refused() {
        let all = [Representation::NgsiLd];
        assert!(deprecation("2026-11-02", "2027-01-31")
            .validate(&all)
            .is_ok());
        let err = deprecation("2026-11-02", "2027-01-30")
            .validate(&all)
            .expect_err("89 days");
        assert!(err.to_string().contains("2027-01-31"), "{err}");
        assert_eq!(field_of(err), "spec.deprecation.sunset");
        let err = deprecation("2026-11-02", "2026-10-01")
            .validate(&all)
            .expect_err("before since");
        assert_eq!(field_of(err), "spec.deprecation.sunset");
    }

    #[test]
    fn a_link_is_a_public_https_url() {
        let all = [Representation::NgsiLd];
        let mut d = deprecation("2026-11-02", "2027-02-01");
        d.link = Some("https://example.org/open-data/v2?x=1".to_owned());
        assert!(d.validate(&all).is_ok());
        for bad in [
            "http://example.org/",
            "https://",
            "https:///path",
            "https://user:pw@example.org/",
            "https://example.org/a b",
            "https://example.org/>; rel=\"x\"",
            "https://example.org/\r\nSet-Cookie: a=b",
            "javascript:alert(1)",
        ] {
            d.link = Some(bad.to_owned());
            let err = d.validate(&all).expect_err(bad);
            assert_eq!(field_of(err), "spec.deprecation.link", "{bad}");
        }
        d.link = Some(format!("https://example.org/{}", "a".repeat(MAX_LINK_LEN)));
        assert!(d.validate(&all).is_err());
    }

    #[test]
    fn a_surface_must_be_served_and_an_attribute_named() {
        let mut d = deprecation("2026-11-02", "2027-02-01");
        d.representations = vec![Representation::OgcFeatures];
        let err = d
            .validate(&[Representation::NgsiLd])
            .expect_err("not served");
        assert_eq!(field_of(err), "spec.deprecation.representations");
        assert!(d
            .validate(&[Representation::NgsiLd, Representation::OgcFeatures])
            .is_ok());
        d.attributes = vec!["id".to_owned()];
        let err = d
            .validate(&[Representation::OgcFeatures])
            .expect_err("system member");
        assert_eq!(field_of(err), "spec.deprecation.attributes");
    }

    #[test]
    fn covers_its_surfaces_or_every_surface() {
        let mut d = deprecation("2026-11-02", "2027-02-01");
        assert!(d.covers(Some(Representation::Sta)));
        assert!(d.covers(None));
        d.representations = vec![Representation::OgcFeatures];
        assert!(d.covers(Some(Representation::OgcFeatures)));
        assert!(!d.covers(Some(Representation::NgsiLd)));
        assert!(!d.covers(None));
    }

    #[test]
    fn unknown_members_are_refused() {
        let err = serde_json::from_str::<Deprecation>(
            r#"{"since":"2026-11-02","sunset":"2027-02-01","note":"internal"}"#,
        )
        .expect_err("unknown field");
        assert!(err.to_string().contains("note"), "{err}");
    }
}
