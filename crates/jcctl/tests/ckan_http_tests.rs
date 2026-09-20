//! The HTTP `CkanApi` against a CKAN that answers from a script (T-0487, EP-62, EP-65,
//! EP-67, CC-18).
//!
//! The fake speaks the Action API the way CKAN 2.10 does — the `success`/`result`
//! envelope, `404` with a Not Found error for a `*_show` that finds nothing, `403` with an
//! Authorization Error for a bad token, the organization answered by id beside a nested
//! `organization`, decorated tags, sorted extras, resources with minted ids — so these
//! exercise the real client and the real create-or-update decision against the shapes a
//! live catalogue answers with, rather than a mock of them.

use jc_core::kinds::ckan::CkanInstanceSpec;
use jcctl::loader::RawManifest;
use jcctl::publish::ckan::{
    publish, withdraw, CkanApi, CkanError, Outcome, PublishError, Settings,
};
use jcctl::publish::ckan_datastore::{self as datastore, PRIMARY_KEY};
use jcctl::publish::ckan_http::HttpCkan;
use jcctl::secrets::SecretValue;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

const TOKEN: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.ckan-api-token-never-published";

// --- the fake catalogue ------------------------------------------------------------------

/// One request the fake answered.
#[derive(Debug, Clone)]
struct Call {
    method: String,
    action: String,
    authorization: Option<String>,
    body: Value,
}

#[derive(Debug, Default)]
struct Catalogue {
    organizations: BTreeSet<String>,
    /// Datasets by name, stored the way `package_show` answers them.
    packages: BTreeMap<String, Value>,
    /// DataStore tables by resource id: fields, then rows by primary key.
    tables: BTreeMap<String, (Vec<Value>, BTreeMap<String, Value>)>,
    calls: Vec<Call>,
    minted: usize,
    /// When set, every answer is a proxy's error page rather than CKAN.
    broken: bool,
}

impl Catalogue {
    fn writes(&self) -> Vec<&str> {
        self.calls
            .iter()
            .filter(|call| call.method == "POST")
            .map(|call| call.action.as_str())
            .collect()
    }

    fn mint(&mut self, prefix: &str) -> String {
        self.minted += 1;
        format!("{prefix}-{:04}", self.minted)
    }

    fn package_by_id_or_name(&self, key: &str) -> Option<(String, Value)> {
        self.packages
            .iter()
            .find(|(name, package)| name.as_str() == key || package["id"] == json!(key))
            .map(|(name, package)| (name.clone(), package.clone()))
    }

    fn resource(&self, id: &str) -> Option<Value> {
        self.packages.values().find_map(|package| {
            package["resources"]
                .as_array()?
                .iter()
                .find(|resource| resource["id"] == json!(id))
                .cloned()
        })
    }

    /// Stores a dataset the way CKAN stores it: ids minted, tags decorated, extras sorted,
    /// the organization answered by id beside its nested record.
    fn store(&mut self, payload: &Value, existing: Option<&Value>) -> Value {
        let name = payload["name"].as_str().unwrap_or_default().to_owned();
        let mut stored = payload.clone();
        stored["id"] = existing
            .and_then(|live| live.get("id").cloned())
            .unwrap_or_else(|| json!(self.mint("pkg")));
        let organization = payload["owner_org"].as_str().unwrap_or_default().to_owned();
        stored["owner_org"] = json!(format!("org-{organization}"));
        stored["organization"] =
            json!({ "name": organization, "id": format!("org-{organization}") });
        stored["state"] = payload.get("state").cloned().unwrap_or(json!("active"));
        stored["metadata_modified"] = json!("2026-09-07T10:00:00.000000");
        let tags: Vec<Value> = payload["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|tag| {
                json!({
                    "name": tag["name"],
                    "display_name": tag["name"],
                    "id": format!("tag-{}", tag["name"].as_str().unwrap_or_default()),
                    "state": "active",
                    "vocabulary_id": null
                })
            })
            .collect();
        stored["tags"] = Value::Array(tags);
        let mut extras: Vec<Value> = payload["extras"].as_array().cloned().unwrap_or_default();
        extras.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
        stored["extras"] = Value::Array(extras);
        let resources: Vec<Value> = payload["resources"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|resource| {
                let mut resource = resource.clone();
                if resource.get("id").is_none() {
                    resource["id"] = json!(self.mint("res"));
                }
                resource["package_id"] = stored["id"].clone();
                if resource.get("url_type").is_none() {
                    resource["url_type"] = Value::Null;
                    resource["datastore_active"] = json!(false);
                }
                resource
            })
            .collect();
        stored["resources"] = Value::Array(resources);
        self.packages.insert(name, stored.clone());
        stored
    }

    fn answer(&mut self, call: &Call, query: &BTreeMap<String, String>) -> (u16, Value) {
        let id = query.get("id").cloned().unwrap_or_default();
        match (call.method.as_str(), call.action.as_str()) {
            ("GET", "organization_show") => match self.organizations.contains(&id) {
                true => ok(json!({ "id": format!("org-{id}"), "name": id, "title": id })),
                false => not_found(),
            },
            ("GET", "package_show") => match self.package_by_id_or_name(&id) {
                Some((_, package)) => ok(package),
                None => not_found(),
            },
            ("GET", "resource_show") => match self.resource(&id) {
                Some(resource) => ok(resource),
                None => not_found(),
            },
            ("GET", "datastore_search") => {
                let resource_id = query.get("resource_id").cloned().unwrap_or_default();
                match self.tables.get(&resource_id) {
                    Some((fields, rows)) => {
                        let mut all = vec![json!({ "id": "_id", "type": "int" })];
                        all.extend(fields.iter().cloned());
                        ok(json!({
                            "resource_id": resource_id,
                            "fields": all,
                            "records": [],
                            "total": rows.len()
                        }))
                    }
                    None => not_found(),
                }
            }
            ("POST", _) if call.authorization.as_deref() != Some(TOKEN) => (
                403,
                json!({
                    "success": false,
                    "error": { "__type": "Authorization Error", "message": "Access denied" }
                }),
            ),
            ("POST", "organization_create") => {
                self.organizations
                    .insert(call.body["name"].as_str().unwrap_or_default().to_owned());
                ok(json!({ "name": call.body["name"] }))
            }
            ("POST", "package_create") => {
                let name = call.body["name"].as_str().unwrap_or_default();
                if self.packages.contains_key(name) {
                    return (
                        409,
                        json!({
                            "success": false,
                            "error": {
                                "__type": "Validation Error",
                                "name": ["That URL is already in use."]
                            }
                        }),
                    );
                }
                let stored = self.store(&call.body, None);
                ok(stored)
            }
            ("POST", "package_update") => {
                let key = call.body["id"]
                    .as_str()
                    .or_else(|| call.body["name"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                let Some((old_name, existing)) = self.package_by_id_or_name(&key) else {
                    return not_found();
                };
                self.packages.remove(&old_name);
                let stored = self.store(&call.body, Some(&existing));
                ok(stored)
            }
            ("POST", "package_delete") => {
                let key = call.body["id"].as_str().unwrap_or_default().to_owned();
                let Some((name, mut package)) = self.package_by_id_or_name(&key) else {
                    return not_found();
                };
                package["state"] = json!("deleted");
                self.packages.insert(name, package);
                ok(Value::Null)
            }
            ("POST", "datastore_create") => {
                let fields: Vec<Value> =
                    call.body["fields"].as_array().cloned().unwrap_or_default();
                if let Some(resource_id) = call.body["resource_id"].as_str() {
                    let Some((existing, _)) = self.tables.get_mut(resource_id) else {
                        return not_found();
                    };
                    existing.extend(fields);
                    return ok(json!({ "resource_id": resource_id }));
                }
                let package_key = call.body["resource"]["package_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let Some((name, mut package)) = self.package_by_id_or_name(&package_key) else {
                    return not_found();
                };
                let resource_id = self.mint("res");
                let resource = json!({
                    "id": resource_id,
                    "package_id": package["id"],
                    "name": call.body["resource"]["name"],
                    "format": call.body["resource"]["format"],
                    "url": format!("http://ckan.test/datastore/dump/{resource_id}"),
                    "url_type": "datastore",
                    "datastore_active": true,
                });
                package["resources"]
                    .as_array_mut()
                    .expect("resources")
                    .push(resource);
                self.packages.insert(name, package);
                self.tables
                    .insert(resource_id.clone(), (fields, BTreeMap::new()));
                ok(json!({ "resource_id": resource_id }))
            }
            ("POST", "datastore_upsert") => {
                let resource_id = call.body["resource_id"].as_str().unwrap_or_default();
                let Some((_, rows)) = self.tables.get_mut(resource_id) else {
                    return not_found();
                };
                for record in call.body["records"].as_array().into_iter().flatten() {
                    let key = record[PRIMARY_KEY].as_str().unwrap_or_default().to_owned();
                    rows.insert(key, record.clone());
                }
                ok(json!({ "resource_id": resource_id }))
            }
            ("POST", "datastore_delete") => {
                let resource_id = call.body["resource_id"].as_str().unwrap_or_default();
                if self.tables.remove(resource_id).is_none() {
                    return not_found();
                }
                ok(json!({ "resource_id": resource_id }))
            }
            _ => (
                400,
                json!({
                    "success": false,
                    "error": { "__type": "Bad request", "message": "Action name not known" }
                }),
            ),
        }
    }
}

fn ok(result: Value) -> (u16, Value) {
    (200, json!({ "success": true, "result": result }))
}

fn not_found() -> (u16, Value) {
    (
        404,
        json!({
            "success": false,
            "error": { "__type": "Not Found Error", "message": "Not found" }
        }),
    )
}

/// One request: the head, then as many body bytes as `Content-Length` announced.
fn read_request(stream: &mut TcpStream) -> String {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(read) = stream.read(&mut chunk) {
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        let text = String::from_utf8_lossy(&raw).into_owned();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let announced: usize = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().to_owned())
                })
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            if body.len() >= announced {
                break;
            }
        }
    }
    String::from_utf8_lossy(&raw).into_owned()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A CKAN that answers on a loopback port. The thread is detached; the test binary ends it.
fn spawn(catalogue: Arc<Mutex<Catalogue>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("a bound address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let request = read_request(&mut stream);
            let (head, body) = request.split_once("\r\n\r\n").unwrap_or((&request, ""));
            let mut lines = head.lines();
            let request_line = lines.next().unwrap_or_default();
            let mut words = request_line.split_whitespace();
            let method = words.next().unwrap_or_default().to_owned();
            let target = words.next().unwrap_or("/");
            let (path, query_text) = target.split_once('?').unwrap_or((target, ""));
            let query: BTreeMap<String, String> = query_text
                .split('&')
                .filter(|pair| !pair.is_empty())
                .filter_map(|pair| {
                    let (key, value) = pair.split_once('=')?;
                    Some((percent_decode(key), percent_decode(value)))
                })
                .collect();
            let authorization = lines.find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("authorization")
                    .then(|| value.trim().to_owned())
            });
            let action = path.strip_prefix("/api/3/action/").unwrap_or("").to_owned();
            let call = Call {
                method,
                action,
                authorization,
                body: serde_json::from_str(body).unwrap_or(Value::Null),
            };

            let (status, body) = {
                let mut catalogue = catalogue.lock().expect("the catalogue");
                catalogue.calls.push(call.clone());
                if catalogue.broken {
                    (
                        502,
                        "<html><body><h1>502 Bad Gateway</h1></body></html>".to_owned(),
                    )
                } else {
                    let (status, envelope) = catalogue.answer(&call, &query);
                    (status, envelope.to_string())
                }
            };
            let content_type = if status == 502 {
                "text/html"
            } else {
                "application/json"
            };
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn catalogue() -> (Arc<Mutex<Catalogue>>, String) {
    let catalogue = Arc::new(Mutex::new(Catalogue {
        organizations: BTreeSet::from(["mesto-banska-bystrica".to_owned()]),
        ..Catalogue::default()
    }));
    let base = spawn(Arc::clone(&catalogue));
    (catalogue, base)
}

/// A client holding `token`, read the way the command reads it: out of the environment,
/// never out of a string a test could have taken from a manifest.
fn client(base: &str, token: &str) -> HttpCkan {
    let variable = format!(
        "JCCTL_TEST_CKAN_TOKEN_{}",
        std::thread::current()
            .name()
            .unwrap_or("main")
            .replace(':', "_")
    );
    std::env::set_var(&variable, token);
    let token = SecretValue::from_env(&variable).expect("the token is set");
    HttpCkan::new(base, token).expect("a client")
}

// --- the endpoint under test ---------------------------------------------------------------

fn endpoint() -> RawManifest {
    serde_norway::from_str(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: ovzdusie-public
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  enabledRepresentations: [ngsi-ld, csv]
  publish:
    ckan:
      instanceRef: { kind: CkanInstance, name: open-data }
      organization: mesto-banska-bystrica
      name: kvalita-ovzdusia
"#,
    )
    .expect("the manifest parses")
}

fn instance() -> CkanInstanceSpec {
    serde_norway::from_str(
        r#"url: https://data.banskabystrica.sk
apiTokenRef: { name: ckan-open-data, key: apiToken }
"#,
    )
    .expect("the instance spec parses")
}

fn record(title: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/dcat.jsonld",
        "@id": "https://data.example.org/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y/",
        "@type": "dcat:Dataset",
        "dct:identifier": "ovzdusie-public",
        "dct:title": [{ "@value": title, "@language": "en" }],
        "dct:description": [{ "@value": "Hourly air quality observations.", "@language": "en" }],
        "dcat:keyword": ["ovzdusie", "air-quality"],
        "dct:language": "sk",
        "dct:publisher": "Mesto Banská Bystrica"
    })
}

fn settings() -> Settings {
    Settings::new("data.example.org").titled("Mesto Banská Bystrica")
}

// --- create, unchanged, update, withdraw -------------------------------------------------

/// EP-62, CC-18: the first run creates the dataset with one `package_create`; the second
/// finds the catalogue's own answer — organization by id, decorated tags, sorted extras,
/// resources with ids — equal to what it would write, and writes nothing.
#[test]
fn the_first_run_creates_the_dataset_over_http_and_the_second_writes_nothing() {
    let (catalogue, base) = catalogue();
    let mut api = client(&base, TOKEN);

    let first = publish(
        &mut api,
        &endpoint(),
        &instance(),
        &record("Air quality"),
        &settings(),
    )
    .expect("the first run publishes");
    assert_eq!(first, Outcome::Created);
    {
        let catalogue = catalogue.lock().expect("the catalogue");
        assert_eq!(catalogue.writes(), vec!["package_create"]);
        let create = catalogue
            .calls
            .iter()
            .find(|call| call.action == "package_create")
            .expect("the creating call");
        assert_eq!(create.authorization.as_deref(), Some(TOKEN));
        assert!(!create.body.to_string().contains(TOKEN), "{}", create.body);
        let stored = catalogue
            .packages
            .get("kvalita-ovzdusia")
            .expect("the dataset");
        assert_eq!(stored["title"], json!("Air quality"));
        assert_eq!(
            stored["organization"]["name"],
            json!("mesto-banska-bystrica")
        );
        // Every GET went to the Action API with the object in the query, not the path.
        assert!(catalogue
            .calls
            .iter()
            .filter(|call| call.method == "GET")
            .all(|call| matches!(call.action.as_str(), "organization_show" | "package_show")));
    }

    let second = publish(
        &mut api,
        &endpoint(),
        &instance(),
        &record("Air quality"),
        &settings(),
    )
    .expect("the second run publishes");
    assert_eq!(second, Outcome::Unchanged);
    assert_eq!(
        catalogue.lock().expect("the catalogue").writes(),
        vec!["package_create"],
        "a converged run writes nothing"
    );
}

/// EP-63: a changed record is one `package_update`, addressed by the id CKAN minted.
#[test]
fn a_changed_record_updates_the_dataset_by_its_id() {
    let (catalogue, base) = catalogue();
    let mut api = client(&base, TOKEN);
    publish(
        &mut api,
        &endpoint(),
        &instance(),
        &record("Air quality"),
        &settings(),
    )
    .expect("the first run publishes");

    let updated = publish(
        &mut api,
        &endpoint(),
        &instance(),
        &record("Air quality, hourly"),
        &settings(),
    )
    .expect("the second run publishes");
    assert_eq!(updated, Outcome::Updated);

    let catalogue = catalogue.lock().expect("the catalogue");
    assert_eq!(catalogue.writes(), vec!["package_create", "package_update"]);
    let update = catalogue
        .calls
        .iter()
        .find(|call| call.action == "package_update")
        .expect("the updating call");
    assert_eq!(update.body["id"], json!("pkg-0001"));
    assert_eq!(
        catalogue.packages["kvalita-ovzdusia"]["title"],
        json!("Air quality, hourly")
    );
}

/// CC-19, CC-18: a withdrawal is one `package_delete`; CKAN keeps the dataset as deleted
/// and a second withdrawal sees that and writes nothing. Publishing it again brings it
/// back as an update, since the name is still taken.
#[test]
fn a_withdrawal_deletes_once_and_a_republication_revives() {
    let (catalogue, base) = catalogue();
    let mut api = client(&base, TOKEN);
    publish(
        &mut api,
        &endpoint(),
        &instance(),
        &record("Air quality"),
        &settings(),
    )
    .expect("the run publishes");

    assert_eq!(
        withdraw(&mut api, "kvalita-ovzdusia").expect("withdrawn"),
        Outcome::Withdrawn
    );
    assert_eq!(
        withdraw(&mut api, "kvalita-ovzdusia").expect("a second withdrawal"),
        Outcome::Unchanged
    );
    assert_eq!(
        withdraw(&mut api, "never-published").expect("nothing to withdraw"),
        Outcome::Unchanged
    );
    assert_eq!(
        catalogue.lock().expect("the catalogue").writes(),
        vec!["package_create", "package_delete"]
    );

    let revived = publish(
        &mut api,
        &endpoint(),
        &instance(),
        &record("Air quality"),
        &settings(),
    )
    .expect("the republication");
    assert_eq!(revived, Outcome::Updated);
    let catalogue = catalogue.lock().expect("the catalogue");
    assert_eq!(
        catalogue.packages["kvalita-ovzdusia"]["state"],
        json!("active")
    );
}

/// A `*_show` CKAN answers `404` is `None`, so the caller creates rather than fails.
#[test]
fn an_object_ckan_does_not_have_is_none() {
    let (_catalogue, base) = catalogue();
    let api = client(&base, TOKEN);

    assert_eq!(
        api.show("package_show", "never-published")
            .expect("answered"),
        None
    );
    assert_eq!(
        api.show("organization_show", "nobody").expect("answered"),
        None
    );
    assert_eq!(
        api.show("resource_show", "no-such-resource")
            .expect("answered"),
        None
    );
    assert_eq!(
        api.show("organization_show", "mesto-banska-bystrica")
            .expect("answered")
            .map(|org| org["name"].clone()),
        Some(json!("mesto-banska-bystrica"))
    );
}

/// EP-67: a bad token is a refusal that repeats CKAN's status and message and nothing
/// of the credential — neither the one sent nor any other.
#[test]
fn a_bad_token_is_a_refusal_without_the_token() {
    let (catalogue, base) = catalogue();
    let mut api = client(&base, "not-the-token");

    let error = publish(
        &mut api,
        &endpoint(),
        &instance(),
        &record("Air quality"),
        &settings(),
    )
    .expect_err("CKAN refused");
    let message = error.to_string();
    assert!(
        matches!(&error, PublishError::Api(CkanError::Rejected { action, .. }) if action == "package_create"),
        "{error:?}"
    );
    assert!(message.contains("403"), "{message}");
    assert!(message.contains("Authorization Error"), "{message}");
    assert!(message.contains("Access denied"), "{message}");
    assert!(!message.contains("not-the-token"), "{message}");
    assert!(!message.contains(TOKEN), "{message}");
    assert!(!format!("{api:?}").contains("not-the-token"), "{api:?}");
    assert!(catalogue.lock().expect("the catalogue").packages.is_empty());
}

/// A catalogue that is not there is unavailable, with the action named.
#[test]
fn a_catalogue_that_does_not_answer_is_unavailable() {
    let mut api = client("http://127.0.0.1:1", TOKEN);
    let error = publish(
        &mut api,
        &endpoint(),
        &instance(),
        &record("Air quality"),
        &settings(),
    )
    .expect_err("nothing listens");
    assert!(
        matches!(&error, PublishError::Api(CkanError::Unavailable(message)) if message.starts_with("organization_show")),
        "{error}"
    );
    assert!(!error.to_string().contains(TOKEN));
}

/// A proxy's error page is not CKAN's envelope: reported as unavailable with the status
/// and a line of what came back, rather than parsed as a dataset.
#[test]
fn a_proxy_error_page_is_unavailable_with_its_status() {
    let (catalogue, base) = catalogue();
    catalogue.lock().expect("the catalogue").broken = true;
    let api = client(&base, TOKEN);

    let error = api
        .show("package_show", "kvalita-ovzdusia")
        .expect_err("a 502 page");
    let message = error.to_string();
    assert!(matches!(error, CkanError::Unavailable(_)), "{message}");
    assert!(message.contains("502"), "{message}");
    assert!(message.contains("Bad Gateway"), "{message}");
}

// --- the DataStore over HTTP ---------------------------------------------------------------

fn page() -> (Vec<String>, Vec<Vec<Value>>) {
    (
        vec!["id".to_owned(), "temperature.value".to_owned()],
        vec![
            vec![
                json!("urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:1"),
                json!(12.5),
            ],
            vec![
                json!("urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:2"),
                json!(13.0),
            ],
        ],
    )
}

/// EP-65, CC-18: the table is created through `datastore_create` and filled through
/// `datastore_upsert`; the resource CKAN minted is found again by id, its fields read
/// through `datastore_search`, and a table that already has them is left alone. The
/// dataset, which now carries the DataStore resource, still reads as unchanged, and an
/// update carries that resource over instead of dropping it.
#[test]
fn the_datastore_mirror_reaches_the_catalogue_and_survives_an_update() {
    let (catalogue, base) = catalogue();
    let mut api = client(&base, TOKEN);
    publish(
        &mut api,
        &endpoint(),
        &instance(),
        &record("Air quality"),
        &settings(),
    )
    .expect("the dataset");
    let (columns, rows) = page();
    let fields = datastore::fields(&columns, &rows, &[]);
    let records = datastore::records(&columns, &rows).expect("records");

    let (resource, outcome) =
        datastore::ensure(&mut api, "kvalita-ovzdusia", "DataStore", &fields).expect("created");
    assert_eq!(outcome, datastore::Outcome::Created);
    let synced = datastore::sync(&mut api, &resource, &[], &records).expect("synced");
    assert_eq!(synced.upserted.len(), 2);
    {
        let catalogue = catalogue.lock().expect("the catalogue");
        assert_eq!(
            catalogue.writes(),
            vec!["package_create", "datastore_create", "datastore_upsert"]
        );
        let (_, stored) = &catalogue.tables[&resource];
        assert_eq!(stored.len(), 2);
    }

    let (again, outcome) =
        datastore::ensure(&mut api, "kvalita-ovzdusia", &resource, &fields).expect("unchanged");
    assert_eq!(
        (again.as_str(), outcome),
        (resource.as_str(), datastore::Outcome::Unchanged)
    );

    assert_eq!(
        publish(
            &mut api,
            &endpoint(),
            &instance(),
            &record("Air quality"),
            &settings()
        )
        .expect("the dataset again"),
        Outcome::Unchanged,
        "the DataStore resource is not drift"
    );
    assert_eq!(
        publish(
            &mut api,
            &endpoint(),
            &instance(),
            &record("Air quality, hourly"),
            &settings()
        )
        .expect("the update"),
        Outcome::Updated
    );
    let catalogue = catalogue.lock().expect("the catalogue");
    let resources = catalogue.packages["kvalita-ovzdusia"]["resources"]
        .as_array()
        .expect("resources")
        .clone();
    assert!(
        resources
            .iter()
            .any(|r| r["id"] == json!(resource) && r["url_type"] == json!("datastore")),
        "{resources:?}"
    );
    drop(catalogue);
    assert_eq!(
        datastore::drop_table(&mut api, &resource).expect("dropped"),
        datastore::Outcome::NotMirrored
    );
}
