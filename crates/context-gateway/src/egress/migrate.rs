//! The one-time move of the subscriptions stored with an R12 notification endpoint to their
//! view (T-3883, plan O-7 (a)).
//!
//! Until T-3653 a subscription was stored on the broker's plain surface with its endpoint
//! rewritten to `{gateway}{base_path}/egress/notifications?to=…&sub=…`, the subscriber sealed
//! with `JC_GATEWAY_DELIVERY_KEY`. Since T-3653 a subscription is stored under the view of its
//! surface with the subscriber's `x-jc-*` headers, and the broker keeps those as `__subject` and
//! delivers itself. This reads every stored one, opens each seal and writes the subscription again
//! that way: same id, same body, its own target. Only code holding the key can say whose
//! subscription it is, so a seal that does not open is reported and left alone, never rewritten
//! to a guess. The Portal's own subscriptions are left to the Portal, which writes them again
//! through the space surface (O-7 (b)). T-3654 deletes this module with R12.

use crate::egress::notifications::{routed_parts, EGRESS_PATH};
use crate::egress::subject::DeliveryKey;
use crate::middleware::tenancy;
use crate::pdp::evaluator::Subject;
use crate::proxy::Broker;
use crate::query;
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::http::header::{ACCEPT, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, Method};
use serde_json::Value;
use std::collections::BTreeMap;

/// One page of the broker's subscription listing.
const PAGE: usize = 100;

/// The most pages one space is read in: a broker that keeps answering full pages is a fault, not
/// a reason to read forever.
const MAX_PAGES: usize = 10_000;

/// The largest listing page or answer this reads.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// Members the broker sets on a stored subscription; a write carrying them is refused.
const SERVER_MEMBERS: &[&str] = &["@context", "createdAt", "modifiedAt", "status"];
const SERVER_NOTIFICATION_MEMBERS: &[&str] = &[
    "status",
    "timesSent",
    "timesFailed",
    "lastNotification",
    "lastSuccess",
    "lastFailure",
];

/// A surface a subscription can have been written through: its space (the broker tenant), the
/// path its R12 endpoint names, and the view it is stored under now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Surface {
    /// The Context Space, the broker tenant.
    pub space: String,
    /// `/api/endpoint/{slug}` or `/cs/{space}`.
    pub base_path: String,
    /// The broker view its requests go to.
    pub view: String,
}

impl Surface {
    /// The view each surface's requests go to (`app::ngsi_ld`, `app::space_ngsi_ld`): an
    /// Endpoint's slug, a Context Space's `cs-{space}`.
    pub fn of(endpoint: &Endpoint) -> Self {
        let view = match endpoint.base_path.starts_with("/cs/") {
            true => format!("cs-{}", endpoint.space),
            false => endpoint.slug.clone(),
        };
        Self {
            space: endpoint.space.clone(),
            base_path: endpoint.base_path.clone(),
            view,
        }
    }
}

/// What one stored R12 subscription is.
#[derive(Debug)]
pub enum Found {
    /// Written by the Portal's reconciler, which writes it again itself (O-7 (b)).
    Reconciler,
    /// Opens under the key: the body to write under `view` for `subject`.
    Rewritable {
        /// The view it is written under.
        view: String,
        /// The subscriber the seal names.
        subject: Box<Subject>,
        /// The subscription as it is written again.
        body: Value,
    },
    /// Nobody can say whose it is; left as it is stored.
    Unreadable(&'static str),
    /// Its target would be refused at creation today (T-1302); left as it is stored.
    Refused(String),
}

/// Whose subscription a stored one is, `None` when its endpoint is no R12 route.
pub fn classify(
    stored: &Value,
    space: &str,
    surfaces: &[Surface],
    key: &DeliveryKey,
    portal_client: &str,
    private_hosts: &[String],
) -> Option<Found> {
    let uri = stored.pointer("/notification/endpoint/uri")?.as_str()?;
    let base_path = reqwest::Url::parse(uri)
        .ok()?
        .path()
        .strip_suffix(EGRESS_PATH)?
        .to_owned();
    let Some((target, areas, sealed)) = routed_parts(uri) else {
        return Some(Found::Unreadable("its route names no HTTP target"));
    };
    let Some(surface) = surfaces
        .iter()
        .find(|surface| surface.space == space && surface.base_path == base_path)
    else {
        return Some(Found::Unreadable(
            "its route names a surface this repository does not hold in its space",
        ));
    };
    let Some(sealed) = sealed else {
        return Some(Found::Unreadable(
            "it was written before subscribers were sealed",
        ));
    };
    let Some(subject) = key.open(&base_path, &target, &areas, &sealed) else {
        return Some(Found::Unreadable("its seal does not open under this key"));
    };
    if subject.service_account.as_deref() == Some(portal_client) {
        return Some(Found::Reconciler);
    }
    if let Some(problem) = crate::notification_target::refusal(&target, private_hosts) {
        return Some(Found::Refused(problem.detail.unwrap_or_default()));
    }
    Some(Found::Rewritable {
        view: surface.view.clone(),
        subject: Box::new(subject),
        body: rewritten(stored, &target),
    })
}

/// The stored subscription as a write: what the broker set taken off, its own target back.
fn rewritten(stored: &Value, target: &str) -> Value {
    let mut body = writable(stored);
    if let Some(uri) = body.pointer_mut("/notification/endpoint/uri") {
        *uri = Value::String(target.to_owned());
    }
    body
}

/// A stored subscription without the members the broker sets.
fn writable(stored: &Value) -> Value {
    let mut body = stored.clone();
    if let Some(members) = body.as_object_mut() {
        for member in SERVER_MEMBERS {
            members.remove(*member);
        }
    }
    if let Some(notification) = body.get_mut("notification").and_then(Value::as_object_mut) {
        for member in SERVER_NOTIFICATION_MEMBERS {
            notification.remove(*member);
        }
    }
    body
}

/// Every R12 subscription of every space the surfaces name, as stored and as found.
#[derive(Debug, Default)]
pub struct Survey {
    /// By space, each R12 subscription as stored and as found.
    pub spaces: BTreeMap<String, Vec<(Value, Found)>>,
}

/// One space's counts, the dry run's answer.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Stored with an R12 endpoint.
    pub routed: usize,
    /// Of those, the Portal's own.
    pub reconciler: usize,
    /// Of those, the ones a real run moves.
    pub rewritable: usize,
    /// Of those, the ones whose seal does not open.
    pub unreadable: usize,
    /// Of those, the ones whose target is refused today.
    pub refused: usize,
}

impl Survey {
    /// The counts per space.
    pub fn counts(&self) -> BTreeMap<&str, Counts> {
        let mut all = BTreeMap::new();
        for (space, found) in &self.spaces {
            let counts: &mut Counts = all.entry(space.as_str()).or_default();
            for (_, found) in found {
                counts.routed += 1;
                match found {
                    Found::Reconciler => counts.reconciler += 1,
                    Found::Rewritable { .. } => counts.rewritable += 1,
                    Found::Unreadable(_) => counts.unreadable += 1,
                    Found::Refused(_) => counts.refused += 1,
                }
            }
        }
        all
    }

    /// The stored form of every R12 subscription, what a real run keeps beside it before it
    /// changes anything.
    pub fn stored(&self) -> Vec<&Value> {
        self.spaces
            .values()
            .flat_map(|found| found.iter().map(|(stored, _)| stored))
            .collect()
    }
}

/// Reads every space's subscriptions and finds the R12 ones. Changes nothing.
pub async fn survey(
    broker: &Broker,
    surfaces: &[Surface],
    key: &DeliveryKey,
    portal_client: &str,
    private_hosts: &[String],
) -> Result<Survey, String> {
    let mut spaces: Vec<&str> = surfaces.iter().map(|s| s.space.as_str()).collect();
    spaces.sort_unstable();
    spaces.dedup();
    let mut survey = Survey::default();
    for space in spaces {
        let mut found = Vec::new();
        for stored in listed(broker, space).await? {
            if let Some(kind) =
                classify(&stored, space, surfaces, key, portal_client, private_hosts)
            {
                found.push((stored, kind));
            }
        }
        survey.spaces.insert(space.to_owned(), found);
    }
    Ok(survey)
}

/// Every subscription the broker holds in one tenant, read page by page.
async fn listed(broker: &Broker, space: &str) -> Result<Vec<Value>, String> {
    let mut all = Vec::new();
    for page in 0..MAX_PAGES {
        let target = format!(
            "/ngsi-ld/v1/subscriptions?limit={PAGE}&offset={}",
            page * PAGE
        );
        let (status, body) = call(broker, Method::GET, &target, plain(space)?, None).await?;
        match status {
            // A space nobody has written to has no tenant at the broker.
            404 if page == 0 => return Ok(all),
            200..=299 => {}
            status => return Err(format!("space {space}: the listing answered {status}")),
        }
        let Value::Array(items) = body else {
            return Err(format!("space {space}: the listing is not a JSON array"));
        };
        let full = items.len() == PAGE;
        all.extend(items);
        if !full {
            return Ok(all);
        }
    }
    Err(format!(
        "space {space}: more than {MAX_PAGES} pages of subscriptions"
    ))
}

/// What happened to one subscription in a real run.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Stored under its view now.
    Rewritten,
    /// Not moved, and why; it is stored as it was unless the reason says otherwise.
    Failed(String),
}

/// Writes every rewritable subscription again under its view. Each is deleted from the plain
/// surface and created under its view with the same id; a create the view refuses puts the
/// original back. The caller keeps [`Survey::stored`] before this runs.
pub async fn apply(broker: &Broker, survey: &Survey) -> Vec<(String, Outcome)> {
    let mut outcomes = Vec::new();
    for (space, found) in &survey.spaces {
        for (stored, found) in found {
            let Found::Rewritable {
                view,
                subject,
                body,
            } = found
            else {
                continue;
            };
            let id = stored
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let outcome = moved(broker, space, view, subject, stored, body, &id).await;
            outcomes.push((id, outcome));
        }
    }
    outcomes
}

async fn moved(
    broker: &Broker,
    space: &str,
    view: &str,
    subject: &Subject,
    stored: &Value,
    body: &Value,
    id: &str,
) -> Outcome {
    if id.is_empty() {
        return Outcome::Failed("the stored subscription names no id".to_owned());
    }
    let by_id = format!("/ngsi-ld/v1/subscriptions/{}", query::encode(id));
    let headers = match plain(space) {
        Ok(headers) => headers,
        Err(reason) => return Outcome::Failed(reason),
    };
    match call(broker, Method::DELETE, &by_id, headers, None).await {
        Ok((200..=299, _)) => {}
        Ok((status, _)) => {
            return Outcome::Failed(format!("the delete answered {status}; left as it was"))
        }
        Err(reason) => return Outcome::Failed(format!("{reason}; left as it was")),
    }
    let created = match as_subscriber(space, subject) {
        Ok(headers) => {
            let target = format!("/x/views/{view}/ngsi-ld/v1/subscriptions");
            call(broker, Method::POST, &target, headers, Some(body)).await
        }
        Err(reason) => Err(reason),
    };
    let refused = match created {
        Ok((200..=299, _)) => return Outcome::Rewritten,
        Ok((status, answer)) => format!(
            "view {view} answered {status}: {}",
            answer
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or_default()
        ),
        Err(reason) => reason,
    };
    // Put back what was there, so a refused move loses nothing.
    let restored = match plain(space) {
        Ok(headers) => {
            let original = writable(stored);
            call(
                broker,
                Method::POST,
                "/ngsi-ld/v1/subscriptions",
                headers,
                Some(&original),
            )
            .await
        }
        Err(reason) => Err(reason),
    };
    match restored {
        Ok((200..=299, _)) => Outcome::Failed(format!("{refused}; restored as it was")),
        Ok((status, _)) => Outcome::Failed(format!(
            "{refused}; restoring answered {status}, the backup holds it"
        )),
        Err(reason) => Outcome::Failed(format!(
            "{refused}; restoring failed ({reason}), the backup holds it"
        )),
    }
}

/// The headers of the gateway's own call on a tenant's plain surface.
fn plain(space: &str) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    let tenant = HeaderValue::from_str(space).map_err(|_| format!("space {space} is no header"))?;
    headers.insert(tenancy::TENANT, tenant);
    tenancy::as_gateway(&mut headers);
    Ok(headers)
}

/// The headers a request of the subscriber carries to the view, pinned as the live path pins
/// them (`tenancy::pin_caller`), so the broker stores the same `__subject`.
fn as_subscriber(space: &str, subject: &Subject) -> Result<HeaderMap, String> {
    let mut request = axum::extract::Request::new(Body::empty());
    tenancy::pin_caller(&mut request, space, subject)
        .map_err(|_| format!("space {space} is no header"))?;
    Ok(std::mem::take(request.headers_mut()))
}

async fn call(
    broker: &Broker,
    method: Method,
    target: &str,
    mut headers: HeaderMap,
    body: Option<&Value>,
) -> Result<(u16, Value), String> {
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    let body = match body {
        Some(body) => {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            Body::from(serde_json::to_vec(body).map_err(|error| error.to_string())?)
        }
        None => Body::empty(),
    };
    let answer = broker
        .send(method, target, headers, body)
        .await
        .map_err(|error| format!("the broker did not answer: {error}"))?;
    let status = answer.status().as_u16();
    let bytes = axum::body::to_bytes(answer.into_body(), MAX_BODY)
        .await
        .map_err(|_| "the broker's answer is unreadable or too large".to_owned())?;
    Ok((
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    ))
}

/// What a run is asked to do: `--portal-client <id>` and one of `--dry-run`, `--backup <file>`.
#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    /// The Portal's client id, whose subscriptions the Portal writes again.
    pub portal_client: String,
    /// Where a real run keeps the stored form first; `None` is the dry run.
    pub backup: Option<std::path::PathBuf>,
}

/// How the subcommand is called.
pub const USAGE: &str =
    "context-gateway migrate-r12-subscriptions --portal-client <id> (--dry-run | --backup <file>)";

impl Args {
    /// The arguments after the subcommand's name.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let (mut portal_client, mut backup, mut dry_run) = (None, None, false);
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--dry-run" => dry_run = true,
                "--portal-client" => portal_client = args.next().cloned(),
                "--backup" => backup = args.next().map(std::path::PathBuf::from),
                other => return Err(format!("unknown argument {other}; {USAGE}")),
            }
        }
        let portal_client = portal_client
            .filter(|client| !client.is_empty())
            .ok_or_else(|| format!("--portal-client names the Portal's client id; {USAGE}"))?;
        match (dry_run, backup) {
            (true, None) => Ok(Self {
                portal_client,
                backup: None,
            }),
            (false, Some(backup)) => Ok(Self {
                portal_client,
                backup: Some(backup),
            }),
            _ => Err(format!("either --dry-run or --backup <file>; {USAGE}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BASE: &str = "/api/endpoint/n4t8xq2vhm6zc9wrb5sdj3kfp7";
    const SPACE: &str = "ovzdusie";
    const PORTAL: &str = "joinedcontext-portal";

    fn key() -> DeliveryKey {
        DeliveryKey::new(&[7u8; 32]).expect("32 bytes")
    }

    fn surfaces() -> Vec<Surface> {
        vec![Surface {
            space: SPACE.to_owned(),
            base_path: BASE.to_owned(),
            view: "n4t8xq2vhm6zc9wrb5sdj3kfp7".to_owned(),
        }]
    }

    fn alice() -> Subject {
        Subject {
            user: Some("alice".to_owned()),
            ..Subject::default()
        }
    }

    /// A subscription as the broker hands it back, routed and sealed the way T-2383 stored it.
    fn stored(target: &str, subject: &Subject, key: &DeliveryKey) -> Value {
        let areas = vec!["POLYGON((0 0,1 0,1 1,0 0))".to_owned()];
        let sealed = key.seal(BASE, target, &areas, subject);
        json!({
            "id": "urn:ngsi-ld:Subscription:ovzdusie:a",
            "type": "Subscription",
            "entities": [{"type": "AirQualityObserved"}],
            "q": "pm10>50",
            "createdAt": "2026-10-01T00:00:00Z",
            "modifiedAt": "2026-10-01T00:00:00Z",
            "status": "active",
            "notification": {
                "attributes": ["pm10"],
                "format": "normalized",
                "timesSent": 4,
                "lastNotification": "2026-10-02T00:00:00Z",
                "status": "ok",
                "endpoint": {
                    "uri": format!(
                        "http://context-gateway:8080{BASE}{EGRESS_PATH}?to={}&area={}&sub={}",
                        query::encode(target),
                        query::encode(&areas[0]),
                        query::encode(&sealed)
                    ),
                    "accept": "application/json"
                }
            }
        })
    }

    fn found(stored: &Value) -> Option<Found> {
        classify(stored, SPACE, &surfaces(), &key(), PORTAL, &[])
    }

    #[test]
    fn a_sealed_subscription_is_rewritten_to_its_own_target_under_its_view() {
        let target = "https://hooks.example.com/in?k=1";
        let Some(Found::Rewritable {
            view,
            subject,
            body,
        }) = found(&stored(target, &alice(), &key()))
        else {
            panic!("rewritable");
        };
        assert_eq!(view, "n4t8xq2vhm6zc9wrb5sdj3kfp7");
        assert_eq!(*subject, alice());
        assert_eq!(body["notification"]["endpoint"]["uri"], target);
        assert_eq!(
            body["notification"]["endpoint"]["accept"],
            "application/json"
        );
        assert_eq!(body["q"], "pm10>50");
        assert_eq!(body["id"], "urn:ngsi-ld:Subscription:ovzdusie:a");
        for member in ["createdAt", "modifiedAt", "status"] {
            assert!(body.get(member).is_none(), "{member}");
        }
        for member in ["timesSent", "lastNotification", "status"] {
            assert!(body["notification"].get(member).is_none(), "{member}");
        }
    }

    #[test]
    fn what_no_one_can_read_is_left_alone_and_says_why() {
        let target = "https://hooks.example.com/in";
        let mut tampered = stored(target, &alice(), &key());
        let uri = tampered["notification"]["endpoint"]["uri"]
            .as_str()
            .expect("uri")
            .replace("hooks.example.com", "evil.example.com");
        tampered["notification"]["endpoint"]["uri"] = Value::String(uri);
        let other_key = DeliveryKey::new(&[9u8; 32]).expect("32 bytes");
        let mut unsealed = stored(target, &alice(), &key());
        let uri = unsealed["notification"]["endpoint"]["uri"]
            .as_str()
            .expect("uri");
        let cut = uri.find("&sub=").expect("sealed");
        unsealed["notification"]["endpoint"]["uri"] = Value::String(uri[..cut].to_owned());
        let mut elsewhere = stored(target, &alice(), &key());
        let uri = elsewhere["notification"]["endpoint"]["uri"]
            .as_str()
            .expect("uri")
            .replace(BASE, "/api/endpoint/gone");
        elsewhere["notification"]["endpoint"]["uri"] = Value::String(uri);
        for (case, stored, why) in [
            ("tampered", tampered, "does not open"),
            (
                "other key",
                stored(target, &alice(), &other_key),
                "does not open",
            ),
            ("unsealed", unsealed, "before subscribers were sealed"),
            ("unknown surface", elsewhere, "does not hold"),
        ] {
            match found(&stored) {
                Some(Found::Unreadable(reason)) => assert!(reason.contains(why), "{case}"),
                other => panic!("{case}: {other:?}"),
            }
        }
    }

    #[test]
    fn the_portals_own_are_left_to_the_portal_and_an_inward_target_is_refused() {
        let portal = Subject {
            service_account: Some(PORTAL.to_owned()),
            ..Subject::default()
        };
        assert!(matches!(
            found(&stored("https://hooks.example.com/in", &portal, &key())),
            Some(Found::Reconciler)
        ));
        assert!(matches!(
            found(&stored("http://169.254.169.254/latest", &alice(), &key())),
            Some(Found::Refused(_))
        ));
    }

    #[test]
    fn a_subscription_that_never_routed_through_the_gateway_is_not_counted() {
        let direct = json!({
            "id": "urn:ngsi-ld:Subscription:ovzdusie:b",
            "notification": {"endpoint": {"uri": "https://hooks.example.com/in"}}
        });
        assert!(found(&direct).is_none());
        assert!(found(&json!({"id": "x"})).is_none());
    }

    #[test]
    fn a_space_surface_is_stored_under_its_space_view() {
        let surface = Surface {
            space: "ovzdusie".to_owned(),
            base_path: "/cs/ovzdusie".to_owned(),
            view: "cs-ovzdusie".to_owned(),
        };
        let sealed = key().seal("/cs/ovzdusie", "https://h.example/in", &[], &alice());
        let stored = json!({
            "id": "urn:ngsi-ld:Subscription:ovzdusie:c",
            "notification": {"endpoint": {"uri": format!(
                "http://gw/cs/ovzdusie{EGRESS_PATH}?to={}&sub={}",
                query::encode("https://h.example/in"),
                query::encode(&sealed)
            )}}
        });
        let Some(Found::Rewritable { view, .. }) =
            classify(&stored, SPACE, &[surface], &key(), PORTAL, &[])
        else {
            panic!("rewritable");
        };
        assert_eq!(view, "cs-ovzdusie");
    }

    #[test]
    fn a_real_run_needs_a_backup_and_the_portal_client() {
        let args = |line: &str| -> Vec<String> { line.split(' ').map(str::to_owned).collect() };
        assert_eq!(
            Args::parse(&args("--portal-client p --dry-run")),
            Ok(Args {
                portal_client: "p".to_owned(),
                backup: None
            })
        );
        assert_eq!(
            Args::parse(&args("--backup /tmp/b.json --portal-client p"))
                .map(|a| a.backup.is_some()),
            Ok(true)
        );
        for refused in [
            "--portal-client p",
            "--dry-run",
            "--portal-client p --dry-run --backup b",
            "--portal-client p --apply",
            "--portal-client",
        ] {
            assert!(Args::parse(&args(refused)).is_err(), "{refused}");
        }
    }
}
