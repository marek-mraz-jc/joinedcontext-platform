//! The access routes as OData 4.01 operations (T-6657): one service root per Endpoint,
//! `/api/endpoint/{slug}/access/odata`, whose `$metadata` the shared `odata-csdl` crate writes.
//!
//! | Plain route                        | OData twin                                  |
//! |------------------------------------|---------------------------------------------|
//! | `GET  access` (AuthZEN JSON)       | `GET  access/odata/Access()`                |
//! | `GET  access/attributes?type=T`    | `GET  access/odata/Attributes(type='T')`    |
//! | `POST access/check`                | `POST access/odata/Check`                   |
//! | `POST access/simulate`             | `POST access/odata/Simulate`                |
//!
//! Each twin calls the plain route's own handler, so the caller check, the rate charge, the
//! input rules (`Simulate` keeps `deny_unknown_fields`) and the answer are the plain route's: a
//! success gains `@odata.context` and the OData headers, a refusal is passed on as it is.

use super::{access, access_check, access_simulate, attributes, the_portal};
use crate::app::{Gateway, MAX_BODY};
use crate::auth::subject::admit;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{ACCEPT, ALLOW, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderValue, Method, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use jc_core::ProblemDetails;
use odata_csdl::{
    write_json, write_xml, ComplexType, CsdlVersion, EntityContainer, Limits, Model, Operation,
    OperationImport, OperationKind, Parameter, Schema, TypeRef,
};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// The namespace of the access operations and their result types.
const NAMESPACE: &str = "Gateway.Access";
/// The OData version every answer of this service declares (`Edm.Untyped` needs 4.01).
const ODATA_VERSION: &str = "4.01";
/// The operations a client can call, as the service document and a `404` name them.
const OPERATIONS: &str = "Access(), Attributes(type='<entity type>'), Check and Simulate";

/// The service's routes; `view_access::routes` merges them beside the plain ones.
pub(crate) fn routes() -> Router<Arc<Gateway>> {
    Router::new()
        .route("/api/endpoint/{slug}/access/odata", any(service))
        .route("/api/endpoint/{slug}/access/odata/", any(service))
        .route(
            "/api/endpoint/{slug}/access/odata/{operation}",
            any(operation),
        )
}

/// The EDM model: four unbound operations, each returning an open complex type, because the
/// documents they answer are the broker engine's and carry what it carries.
pub(crate) fn model() -> Model {
    let qualified = |name: &str| format!("{NAMESPACE}.{name}");
    let untyped = |name: &str| Parameter::new(name, TypeRef::single("Edm.Untyped"));
    let mut schema = Schema::new(NAMESPACE);
    schema.complex_types = ["AccessDocument", "AttributeTable", "Decision"]
        .into_iter()
        .map(|name| ComplexType {
            open: true,
            ..ComplexType::new(name)
        })
        .collect();
    let mut access = Operation::new("Access", OperationKind::Function);
    access.return_type = Some(TypeRef::single(qualified("AccessDocument")));
    let mut table = Operation::new("Attributes", OperationKind::Function);
    table.parameters = vec![Parameter::new("type", TypeRef::single("Edm.String")).required()];
    table.return_type = Some(TypeRef::single(qualified("AttributeTable")));
    let mut check = Operation::new("Check", OperationKind::Action);
    check.parameters = vec![
        untyped("subject"),
        untyped("action").required(),
        untyped("resource"),
        untyped("context"),
    ];
    check.return_type = Some(TypeRef::single(qualified("Decision")));
    let mut simulate = Operation::new("Simulate", OperationKind::Action);
    simulate.parameters = vec![
        untyped("subject").required(),
        untyped("action"),
        untyped("resource"),
    ];
    simulate.return_type = Some(TypeRef::single(qualified("Decision")));
    let mut container = EntityContainer::new("Service");
    container.imports = ["Access", "Attributes", "Check", "Simulate"]
        .into_iter()
        .map(|name| OperationImport::new(name, qualified(name)))
        .collect();
    schema.operations = vec![access, table, check, simulate];
    schema.entity_container = Some(container);
    Model {
        version: CsdlVersion::V4_01,
        references: Vec::new(),
        schemas: vec![schema],
    }
}

/// The service root of `slug` as the caller reaches it.
fn root(gateway: &Gateway, slug: &str) -> String {
    format!("{}/api/endpoint/{slug}/access/odata", gateway.base_url())
}

/// The service document and `$metadata` describe the Endpoint: a caller admitted to it, or the
/// Portal (which calls `Simulate`), may read them; anyone else learns nothing, not even that
/// the slug exists, exactly as on `access`.
fn described_to(
    gateway: &Gateway,
    slug: &str,
    request: &Request,
) -> Result<(), Box<Response<Body>>> {
    match admit(gateway, slug, None, request.headers()) {
        Ok(_) => Ok(()),
        Err(_) if the_portal(gateway, request.headers()).is_ok() => Ok(()),
        Err(problem) => Err(problem),
    }
}

/// `GET access/odata`: the service document, listing the two functions.
async fn service(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::GET {
        return not_allowed("GET");
    }
    if let Err(refused) = described_to(&gateway, &slug, &request) {
        return *refused;
    }
    let root = root(&gateway, &slug);
    let document = json!({
        "@odata.context": format!("{root}/$metadata"),
        "value": [
            { "name": "Access", "kind": "FunctionImport", "url": "Access" },
            { "name": "Attributes", "kind": "FunctionImport", "url": "Attributes" },
        ],
    });
    odata_json(document.to_string().into_bytes())
}

/// One operation, `$metadata` or an unknown name of the service root.
async fn operation(
    State(gateway): State<Arc<Gateway>>,
    Path((slug, name)): Path<(String, String)>,
    mut request: Request,
) -> Response<Body> {
    let method = request.method().clone();
    let context = |ty: &str| format!("{}/$metadata#{NAMESPACE}.{ty}", root(&gateway, &slug));
    match name.as_str() {
        "$metadata" if method == Method::GET => metadata(&gateway, &slug, &request),
        "Access()" if method == Method::GET => {
            // The OData twin answers the AuthZEN JSON document; ODRL and Turtle stay `access`'s.
            request
                .headers_mut()
                .insert(ACCEPT, HeaderValue::from_static("application/json"));
            let answer = access(State(Arc::clone(&gateway)), Path(slug.clone()), request).await;
            odata_answer(answer, &context("AccessDocument")).await
        }
        "Check" if method == Method::POST => {
            let answer =
                access_check(State(Arc::clone(&gateway)), Path(slug.clone()), request).await;
            odata_answer(answer, &context("Decision")).await
        }
        "Simulate" if method == Method::POST => {
            let answer =
                access_simulate(State(Arc::clone(&gateway)), Path(slug.clone()), request).await;
            odata_answer(answer, &context("Decision")).await
        }
        _ if name.starts_with("Attributes(") && method == Method::GET => {
            let aliases = Query::<HashMap<String, String>>::try_from_uri(request.uri())
                .map(|query| query.0)
                .unwrap_or_default();
            let asked = match type_parameter(&name, &aliases) {
                Ok(asked) => asked,
                Err(detail) => {
                    return ProblemDetails::bad_request()
                        .with_detail(detail)
                        .into_response()
                }
            };
            let answer = attributes(&gateway, &slug, request, Some(asked)).await;
            odata_answer(answer, &context("AttributeTable")).await
        }
        "$metadata" | "Access()" => not_allowed("GET"),
        "Check" | "Simulate" => not_allowed("POST"),
        _ if name.starts_with("Attributes(") => not_allowed("GET"),
        _ => ProblemDetails::not_found()
            .with_detail(format!("this service answers {OPERATIONS}"))
            .into_response(),
    }
}

/// The `type` of `Attributes(type='T')` or `Attributes(type=@t)?@t='T'`, a string literal with
/// `''` for a quote (OData URL conventions 5.1.1.6.1, 11.5.4.1.1).
fn type_parameter(segment: &str, aliases: &HashMap<String, String>) -> Result<String, String> {
    let usage = "the function is called as Attributes(type='AirQualityObserved')";
    let inner = segment
        .strip_prefix("Attributes(")
        .and_then(|rest| rest.strip_suffix(')'))
        .ok_or_else(|| usage.to_owned())?;
    let value = inner
        .strip_prefix("type=")
        .ok_or_else(|| format!("type is its one parameter: {usage}"))?;
    let literal = match value.strip_prefix('@') {
        Some(alias) => aliases
            .get(&format!("@{alias}"))
            .ok_or_else(|| format!("the parameter alias @{alias} has no value"))?
            .as_str(),
        None => value,
    };
    let quoted = literal
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .ok_or_else(|| format!("type is a string literal in single quotes: {usage}"))?;
    if quoted.replace("''", "").contains('\'') {
        return Err(format!("a quote inside type is written twice: {usage}"));
    }
    Ok(quoted.replace("''", "'"))
}

/// `$metadata` in CSDL XML, or CSDL JSON for `$format=json` or a JSON-only `Accept`.
fn metadata(gateway: &Gateway, slug: &str, request: &Request) -> Response<Body> {
    if let Err(refused) = described_to(gateway, slug, request) {
        return *refused;
    }
    let format = Query::<HashMap<String, String>>::try_from_uri(request.uri())
        .ok()
        .and_then(|query| query.0.get("$format").cloned());
    let accept = request
        .headers()
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let json = match format.as_deref() {
        Some("json" | "application/json") => true,
        Some("xml" | "application/xml") => false,
        Some(other) => {
            return ProblemDetails::for_status(406)
                .with_detail(format!(
                    "$format {other:?} is not served: $metadata is json or xml"
                ))
                .into_response()
        }
        None => accept.contains("application/json") && !accept.contains("xml"),
    };
    let mut out = Vec::new();
    let written = if json {
        write_json(&model(), &Limits::strict(), &mut out)
    } else {
        write_xml(&model(), &Limits::strict(), &mut out)
    };
    if let Err(error) = written {
        tracing::error!(%error, "the access service model does not write");
        return ProblemDetails::internal().into_response();
    }
    let content_type = if json {
        "application/json"
    } else {
        "application/xml"
    };
    let mut answer = (StatusCode::OK, out).into_response();
    let headers = answer.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert("odata-version", HeaderValue::from_static(ODATA_VERSION));
    answer
}

/// The plain route's `answer` as the OData twin's: a JSON object success gains
/// `@odata.context` first, every other answer (a refusal above all) is passed on unchanged.
async fn odata_answer(answer: Response<Body>, context: &str) -> Response<Body> {
    let is_json = answer
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !answer.status().is_success() || !is_json {
        return answer;
    }
    let (mut parts, body) = answer.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        tracing::error!("an access answer passed the gateway's own size cap");
        return ProblemDetails::internal().into_response();
    };
    let Ok(Value::Object(members)) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    let mut document = Map::with_capacity(members.len().saturating_add(1));
    document.insert(
        "@odata.context".to_owned(),
        Value::String(context.to_owned()),
    );
    document.extend(members);
    let Ok(written) = serde_json::to_vec(&Value::Object(document)) else {
        return ProblemDetails::internal().into_response();
    };
    parts.headers.remove(CONTENT_LENGTH);
    let mut answer = Response::from_parts(parts, Body::from(written));
    odata_headers(&mut answer);
    answer
}

fn odata_json(body: Vec<u8>) -> Response<Body> {
    let mut answer = (StatusCode::OK, body).into_response();
    odata_headers(&mut answer);
    answer
}

fn odata_headers(answer: &mut Response<Body>) {
    let headers = answer.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json;odata.metadata=minimal"),
    );
    headers.insert("odata-version", HeaderValue::from_static(ODATA_VERSION));
}

fn not_allowed(allowed: &'static str) -> Response<Body> {
    let mut answer = ProblemDetails::for_status(405)
        .with_detail(format!("this resource answers {allowed} only"))
        .into_response();
    answer
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static(allowed));
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_is_valid_csdl_in_both_representations() {
        let (mut xml, mut json) = (Vec::new(), Vec::new());
        assert!(write_xml(&model(), &Limits::strict(), &mut xml).is_ok());
        assert!(write_json(&model(), &Limits::strict(), &mut json).is_ok());
        let xml = String::from_utf8(xml).unwrap_or_default();
        assert!(xml.contains(r#"<ActionImport Name="Simulate" Action="Gateway.Access.Simulate"/>"#));
        assert!(xml.contains(r#"<Parameter Name="type" Type="Edm.String" Nullable="false"/>"#));
    }

    #[test]
    fn the_type_parameter_is_a_quoted_literal_or_an_alias() {
        let none = HashMap::new();
        let aliased = HashMap::from([("@t".to_owned(), "'Air''s'".to_owned())]);
        assert_eq!(
            type_parameter("Attributes(type='AirQualityObserved')", &none),
            Ok("AirQualityObserved".to_owned())
        );
        assert_eq!(
            type_parameter("Attributes(type=@t)", &aliased),
            Ok("Air's".to_owned())
        );
        for refused in [
            "Attributes()",
            "Attributes(type=Air)",
            "Attributes(kind='Air')",
            "Attributes(type='Air'",
            "Attributes(type='a'b')",
            "Attributes(type=@missing)",
        ] {
            assert!(type_parameter(refused, &aliased).is_err(), "{refused}");
        }
    }
}
