//! The JSON-LD contexts a caller may send through the gateway (T-3287).
//!
//! The gateway decides on short names: a Policy grants `pm10`, a public form's grant names its
//! fields, a projection hides `stationNote`, all as the NGSI-LD core context and the broker's
//! default vocabulary expand them. A context of the caller's own, inline in the body or named by
//! a `Link` header, makes the broker expand the same short names to other IRIs, so what the
//! gateway allowed and what the broker writes or answers would differ. Only the NGSI-LD core
//! context is accepted, which is the broker's default and remaps nothing; every other context
//! is refused before the broker is asked.

use axum::http::header::LINK;
use axum::http::HeaderMap;
use serde_json::Value;

/// The `rel` of a `Link` that names a JSON-LD context (CIM 009 clause 6.3.5).
const CONTEXT_REL: &str = "http://www.w3.org/ns/json-ld#context";

/// Why a request's context was refused, in words the caller can act on.
pub const REFUSAL: &str = "only the NGSI-LD core context is accepted here: the names are \
    the space's, as its model defines them, so send none or the core context and use the \
    attribute names the Endpoint's schema lists";

/// Whether `url` is an NGSI-LD core context, any published version:
/// `https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context[-v1.N].jsonld`.
pub fn is_core_context(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context")
    else {
        return false;
    };
    let Some(version) = rest.strip_suffix(".jsonld") else {
        return false;
    };
    version.is_empty()
        || version
            .strip_prefix("-v1.")
            .is_some_and(|minor| !minor.is_empty() && minor.bytes().all(|b| b.is_ascii_digit()))
}

/// Refuses a `Link` header naming a context other than the core one.
pub fn check_link(headers: &HeaderMap) -> Result<(), &'static str> {
    for value in headers.get_all(LINK) {
        let Ok(value) = value.to_str() else {
            return Err(REFUSAL);
        };
        for link in value.split(',') {
            let (target, params) = match link.split_once(';') {
                Some((target, params)) => (target.trim(), params),
                None => (link.trim(), ""),
            };
            let names_context = params.split(';').any(|param| {
                param.split_once('=').is_some_and(|(key, value)| {
                    key.trim().eq_ignore_ascii_case("rel")
                        && value
                            .trim()
                            .trim_matches('"')
                            .split_whitespace()
                            .any(|rel| rel == CONTEXT_REL)
                })
            });
            if !names_context {
                continue;
            }
            let url = target.trim_start_matches('<').trim_end_matches('>');
            if !is_core_context(url) {
                return Err(REFUSAL);
            }
        }
    }
    Ok(())
}

/// Refuses a JSON body carrying a context other than the core one: at an entity's (or a query's,
/// or a subscription's) own level only the core context, by URL or as a list of URLs; deeper, no
/// `@context` at all, since an embedded context renames what is inside it. A body that is not
/// JSON is left to the checks that refuse it.
pub fn check_body(body: &[u8]) -> Result<(), &'static str> {
    if body.is_empty() {
        return Ok(());
    }
    let Ok(document) = serde_json::from_slice::<Value>(body) else {
        return Ok(());
    };
    let tops: Vec<&Value> = match &document {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    for top in tops {
        let Value::Object(members) = top else {
            continue;
        };
        for (name, value) in members {
            if name == "@context" {
                let core = match value {
                    Value::String(url) => is_core_context(url),
                    Value::Array(urls) => urls
                        .iter()
                        .all(|url| url.as_str().is_some_and(is_core_context)),
                    _ => false,
                };
                if !core {
                    return Err(REFUSAL);
                }
            } else if embeds_context(value) {
                return Err(REFUSAL);
            }
        }
    }
    Ok(())
}

/// Whether `value` holds an `@context` anywhere inside it.
fn embeds_context(value: &Value) -> bool {
    match value {
        Value::Object(members) => members
            .iter()
            .any(|(name, inner)| name == "@context" || embeds_context(inner)),
        Value::Array(items) => items.iter().any(embeds_context),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;

    const CORE: &str = "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld";

    #[test]
    fn the_core_context_of_any_version_is_the_core_context() {
        for url in [
            CORE,
            "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context.jsonld",
            "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.9.jsonld",
        ] {
            assert!(is_core_context(url), "{url}");
        }
        for url in [
            "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.jsonld",
            "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld?x",
            "http://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context.jsonld",
            "https://uri.etsi.org.example/ngsi-ld/v1/ngsi-ld-core-context.jsonld",
            "https://example.org/context.jsonld",
        ] {
            assert!(!is_core_context(url), "{url}");
        }
    }

    fn link(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(LINK, HeaderValue::from_str(value).expect("header"));
        headers
    }

    #[test]
    fn a_link_names_the_core_context_or_is_refused() {
        assert!(check_link(&HeaderMap::new()).is_ok());
        assert!(check_link(&link(&format!(
            "<{CORE}>; rel=\"{CONTEXT_REL}\"; type=\"application/ld+json\""
        )))
        .is_ok());
        assert!(check_link(&link(
            "<https://example.org/c.jsonld>; rel=\"http://www.w3.org/ns/json-ld#context\""
        ))
        .is_err());
        // A second link in the same header, and a bare rel, are read too.
        assert!(check_link(&link(&format!(
            "<{CORE}>; rel=\"{CONTEXT_REL}\", <https://example.org/c.jsonld>; rel={CONTEXT_REL}"
        )))
        .is_err());
        // A Link of another relation names no context.
        assert!(check_link(&link("<https://example.org/next>; rel=\"next\"")).is_ok());
    }

    #[test]
    fn a_body_carries_the_core_context_or_none() {
        let ok = |body: Value| check_body(body.to_string().as_bytes()).is_ok();
        assert!(ok(
            json!({ "id": "urn:ngsi-ld:Report:1", "type": "Report" })
        ));
        assert!(ok(
            json!({ "id": "urn:ngsi-ld:Report:1", "type": "Report", "@context": CORE })
        ));
        assert!(ok(json!([{ "type": "Report", "@context": [CORE] }])));
        assert!(!ok(
            json!({ "type": "Report", "@context": { "description": "https://example.org/x" } })
        ));
        assert!(!ok(
            json!({ "type": "Report", "@context": [CORE, "https://example.org/c.jsonld"] })
        ));
        assert!(!ok(
            json!([{ "type": "Report" }, { "type": "Report", "@context": "https://example.org/c" }])
        ));
        // An embedded context renames what is inside it, whatever it names.
        assert!(!ok(
            json!({ "type": "Report", "description": { "type": "Property", "value": 1, "@context": CORE } })
        ));
        assert!(check_body(b"").is_ok());
        assert!(
            check_body(b"not json").is_ok(),
            "left to the checks that refuse it"
        );
    }
}
