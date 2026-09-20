//! T-0176: a peer's schema surface is mirrored as read-only foreign DataModels (DM-48, DM-49).
//!
//! The peer is a map from URL to document, which is all the module asks of a transport. What
//! the tests are about is the judgement in front of it: what is fetched, what is refused, and
//! what a second run does when the peer has published something new.

use jcctl::foreign_models::{
    self, FetchError, Mirror, MirrorError, Outcome, SchemaApi, CONTEXT, EXAMPLE, LINKML,
};
use jcctl::loader::{RawManifest, Repository};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const BASE: &str = "https://peer.example/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y";
const LINKML_BODY: &str =
    "id: https://peer.example/air\nname: air\nclasses:\n  AirQualityObserved:\n";
const CONTEXT_BODY: &str = r#"{"@context":{"pm10":"https://peer.example/air#pm10"}}"#;
const EXAMPLE_BODY: &str = r#"{"id":"urn:ngsi-ld:AirQualityObserved:peer:air:1"}"#;

/// A peer that answers from a fixed table and records what it was asked for.
struct Peer {
    documents: BTreeMap<String, Vec<u8>>,
    asked: RefCell<Vec<String>>,
}

impl SchemaApi for Peer {
    fn get(&self, url: &str) -> Result<Option<Vec<u8>>, FetchError> {
        self.asked.borrow_mut().push(url.to_owned());
        Ok(self.documents.get(url).cloned())
    }
}

impl Peer {
    /// A peer publishing one model in three documents, with a catalogue that vouches for them.
    fn air() -> Self {
        Self::with(LINKML_BODY, |artifacts| {
            artifacts.insert(EXAMPLE.to_owned(), descriptor(EXAMPLE_BODY));
        })
    }

    /// The same peer, with the catalogue and the LinkML document under the caller's control.
    fn with(linkml: &str, extra: impl Fn(&mut serde_json::Map<String, Value>)) -> Self {
        let mut artifacts = serde_json::Map::new();
        artifacts.insert(LINKML.to_owned(), descriptor(linkml));
        artifacts.insert(CONTEXT.to_owned(), descriptor(CONTEXT_BODY));
        extra(&mut artifacts);

        let index = json!({
            "endpoint": "zt4qm7ge2xdv6ksb3ncf5arw2y",
            "models": [{
                "name": "air",
                "version": 1,
                "semver": "1.2.0",
                "types": ["AirQualityObserved"],
                "artifacts": artifacts,
            }],
        });

        let mut documents = BTreeMap::new();
        documents.insert(
            format!("{BASE}/schema/index.json"),
            serde_json::to_vec(&index).expect("the catalogue serializes"),
        );
        documents.insert(
            format!("{BASE}/schema/v1/{LINKML}"),
            linkml.as_bytes().to_vec(),
        );
        documents.insert(
            format!("{BASE}/schema/v1/{CONTEXT}"),
            CONTEXT_BODY.as_bytes().to_vec(),
        );
        documents.insert(
            format!("{BASE}/schema/v1/{EXAMPLE}"),
            EXAMPLE_BODY.as_bytes().to_vec(),
        );
        Self {
            documents,
            asked: RefCell::new(Vec::new()),
        }
    }

    /// The same peer with one document removed from the table, the catalogue untouched.
    fn without(mut self, url: &str) -> Self {
        self.documents.remove(url);
        self
    }
}

fn descriptor(body: &str) -> Value {
    json!({ "type": "text/plain", "bytes": body.len(), "sha256": sha256(body) })
}

fn sha256(body: &str) -> String {
    Sha256::digest(body.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The reference the mirror hangs off: a local alias for a partner's space.
fn reference() -> RawManifest {
    serde_norway::from_str(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: SharedSpaceReference
metadata:
  name: partner
  namespace: ovzdusie
spec:
  endpointSlug: zt4qm7ge2xdv6ksb3ncf5arw2y
  alias: partner-air
"#,
    )
    .expect("the reference parses")
}

/// An empty repository in its own directory.
fn repo_dir(test: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("jcctl-mirror-{test}-{now}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the repository");
    dir
}

fn load(dir: &Path) -> Repository {
    Repository::load(dir).expect("the repository loads")
}

fn run(dir: &Path, peer: &Peer) -> Mirror {
    foreign_models::mirror(&reference(), BASE, "2026-09-07T08:00:00Z", &load(dir), peer)
        .expect("the mirror runs")
}

/// A mirror of an existing model, as a previous run would have left it.
fn existing_mirror(dir: &Path, sha256: &str) {
    let path = dir.join("projects/ovzdusie/spaces/partner-air/datamodels");
    std::fs::create_dir_all(&path).expect("create the datamodel directory");
    std::fs::write(
        path.join("partner-air.yaml"),
        format!(
            r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: partner-air
  namespace: ovzdusie
spec:
  contextSpaceRef: partner-air
  linkml: partner-air/model.linkml.yaml
  version: 1.2.0
  lifecycle: mirrored
  classes: [AirQualityObserved]
  source:
    remote:
      url: {BASE}/schema/v1/model.linkml.yaml
      version: 1.2.0
      sha256: {sha256}
      fetchedAt: 2026-09-01T08:00:00Z
  artifacts:
    context: partner-air/context.jsonld
"#
        ),
    )
    .expect("write the previous mirror");
}

/// DM-48: the peer's catalogue becomes a foreign DataModel that pins what was fetched.
#[test]
fn a_peer_model_becomes_a_read_only_foreign_data_model() {
    let dir = repo_dir("created");
    let mirror = run(&dir, &Peer::air());

    assert!(mirror.flags.is_empty(), "{:?}", mirror.flags);
    assert_eq!(mirror.models.len(), 1);
    let model = &mirror.models[0];
    assert_eq!(model.outcome, Outcome::Created);
    assert_eq!(model.manifest.metadata.name, "partner-air");
    assert_eq!(
        model.manifest.metadata.namespace.as_deref(),
        Some("ovzdusie")
    );
    assert_eq!(
        model.path,
        PathBuf::from("projects/ovzdusie/spaces/partner-air/datamodels/partner-air.yaml")
    );

    let spec: jc_core::kinds::DataModelSpec =
        serde_json::from_value(model.manifest.spec.clone()).expect("the mirror is a DataModel");
    assert!(spec.validate().is_ok(), "{:?}", spec.validate());
    assert_eq!(spec.lifecycle, jc_core::kinds::DataModelLifecycle::Mirrored);
    assert_eq!(spec.context_space_ref, "partner-air");
    assert_eq!(spec.classes, vec!["AirQualityObserved".to_owned()]);
    // DM-49: a mirror is nobody's own model, and the lifecycle says so on its own.
    assert!(!spec.can_be_referenced());

    let remote = spec.source.expect("provenance").remote.expect("remote");
    assert_eq!(remote.url, format!("{BASE}/schema/v1/{LINKML}"));
    assert_eq!(remote.version.as_str(), "1.2.0");
    assert_eq!(remote.sha256, sha256(LINKML_BODY));
    assert_eq!(remote.fetched_at.to_rfc3339(), "2026-09-07T08:00:00+00:00");

    // The three documents land beside the manifest, in a directory of the mirror's own name,
    // so a peer's file names can never collide with a local model's.
    let beside = "projects/ovzdusie/spaces/partner-air/datamodels/partner-air";
    let files: Vec<String> = model
        .files
        .keys()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        files,
        vec![
            format!("{beside}/{CONTEXT}"),
            format!("{beside}/{EXAMPLE}"),
            format!("{beside}/{LINKML}"),
        ]
    );
    assert_eq!(
        model.files[&PathBuf::from(format!("{beside}/{LINKML}"))],
        LINKML_BODY.as_bytes()
    );
}

/// DM-48: a peer without a schema surface is flagged, and the reference stands without a model.
#[test]
fn a_peer_without_a_schema_surface_is_flagged_and_still_referenced() {
    let dir = repo_dir("no-surface");
    let peer = Peer::air().without(&format!("{BASE}/schema/index.json"));
    let mirror = run(&dir, &peer);

    assert!(mirror.models.is_empty());
    assert_eq!(mirror.flags.len(), 1);
    assert!(mirror.flags[0].contains("publishes no schema/index.json"));
}

/// The peer's own model name reaches the filesystem, so it is checked before anything is asked
/// for: a name that is not a DNS-1123 label costs one request and writes nothing.
#[test]
fn a_peer_model_name_that_is_not_a_label_never_reaches_the_filesystem() {
    let dir = repo_dir("bad-name");
    let mut peer = Peer::air();
    let index = json!({
        "endpoint": "zt4qm7ge2xdv6ksb3ncf5arw2y",
        "models": [{ "name": "../../../etc/passwd", "version": 1, "semver": "1.0.0" }],
    });
    peer.documents.insert(
        format!("{BASE}/schema/index.json"),
        serde_json::to_vec(&index).expect("serializes"),
    );

    let mirror = run(&dir, &peer);
    assert!(mirror.models.is_empty());
    assert_eq!(mirror.flags.len(), 1);
    assert!(mirror.flags[0].contains("not a DNS-1123 label"));
    assert_eq!(
        peer.asked.borrow().len(),
        1,
        "only the catalogue was fetched: {:?}",
        peer.asked.borrow()
    );
}

/// A document the peer's own catalogue does not vouch for is refused, not written.
#[test]
fn a_document_that_does_not_match_the_catalogue_is_refused() {
    let dir = repo_dir("digest");
    let mut peer = Peer::air();
    peer.documents.insert(
        format!("{BASE}/schema/v1/{LINKML}"),
        b"name: something-else\n".to_vec(),
    );

    let mirror = run(&dir, &peer);
    assert!(mirror.models.is_empty());
    assert_eq!(mirror.flags.len(), 1);
    assert!(
        mirror.flags[0].contains("does not match the digest"),
        "{}",
        mirror.flags[0]
    );
}

/// A surface in the clear is refused before a single request leaves this process.
#[test]
fn a_surface_in_the_clear_is_refused_before_anything_is_asked_for() {
    let dir = repo_dir("plaintext");
    let peer = Peer::air();
    let error = foreign_models::mirror(
        &reference(),
        "http://peer.example/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y",
        "2026-09-07T08:00:00Z",
        &load(&dir),
        &peer,
    )
    .expect_err("plaintext is refused");

    assert!(matches!(error, MirrorError::InsecureBase(_)), "{error}");
    assert!(peer.asked.borrow().is_empty());
}

/// CC-18: a second run over an unchanged peer writes nothing at all.
#[test]
fn an_unchanged_mirror_writes_nothing() {
    let dir = repo_dir("unchanged");
    existing_mirror(&dir, &sha256(LINKML_BODY));

    let mirror = run(&dir, &Peer::air());
    assert_eq!(mirror.models.len(), 1);
    assert_eq!(mirror.models[0].outcome, Outcome::Unchanged);
    assert_eq!(mirror.models[0].previous_sha256, None);
    assert_eq!(foreign_models::write(&dir, &mirror).expect("write runs"), 0);
}

/// DM-49: a changed digest carries the one the repository pinned and the Mappings reading it,
/// which is what the merge request the reconciler opens has to show.
#[test]
fn a_changed_digest_carries_the_previous_one_and_the_mappings_that_read_it() {
    let dir = repo_dir("changed");
    let stale = "0".repeat(64);
    existing_mirror(&dir, &stale);
    let mappings = dir.join("projects/ovzdusie/spaces/partner-air/datamodels/mappings");
    std::fs::create_dir_all(&mappings).expect("create the mappings directory");
    std::fs::write(
        mappings.join("partner-to-local.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Mapping
metadata:
  name: partner-to-local
  namespace: ovzdusie
spec:
  contextSpaceRef: partner-air
  source: { kind: DataModel, name: partner-air, version: "1" }
  target: { kind: DataModel, name: ovzdusie-air, version: "1" }
  transformation: {}
  tests: []
"#,
    )
    .expect("write the mapping");

    let mirror = run(&dir, &Peer::air());
    let model = &mirror.models[0];
    assert_eq!(model.outcome, Outcome::Changed);
    assert_eq!(model.previous_sha256.as_deref(), Some(stale.as_str()));
    assert_eq!(model.affected_mappings, vec!["partner-to-local".to_owned()]);
    assert_eq!(mirror.to_json()["summary"]["changed"], json!(1));
}

/// What `write` puts on disk is what the loader reads back as a mirrored DataModel.
#[test]
fn what_is_written_is_what_the_loader_reads_back() {
    let dir = repo_dir("roundtrip");
    let mirror = run(&dir, &Peer::air());
    assert_eq!(foreign_models::write(&dir, &mirror).expect("write runs"), 4);

    let repo = load(&dir);
    let (_, resource) = repo
        .iter()
        .find(|(id, _)| id.kind == "DataModel" && id.name == "partner-air")
        .expect("the mirror is in the repository");
    let spec: jc_core::kinds::DataModelSpec =
        serde_json::from_value(resource.manifest.spec.clone()).expect("it is a DataModel");
    assert_eq!(spec.lifecycle, jc_core::kinds::DataModelLifecycle::Mirrored);

    let beside = dir.join("projects/ovzdusie/spaces/partner-air/datamodels/partner-air");
    assert_eq!(
        std::fs::read_to_string(beside.join(LINKML)).expect("the source is on disk"),
        LINKML_BODY
    );
    assert_eq!(
        std::fs::read_to_string(beside.join(EXAMPLE)).expect("the example is on disk"),
        EXAMPLE_BODY
    );
    // The authoring source is not a manifest, and the loader knows it: the repository holds
    // exactly the one resource this mirror added.
    assert_eq!(repo.len(), 1);
}

/// DM-48 pins a sha256, so a model the peer does not publish a digest for is not mirrored.
#[test]
fn a_model_the_peer_does_not_vouch_for_is_skipped() {
    let dir = repo_dir("unpinned");
    let mut peer = Peer::air();
    let mut artifacts = serde_json::Map::new();
    artifacts.insert(CONTEXT.to_owned(), descriptor(CONTEXT_BODY));
    let index = json!({
        "models": [{
            "name": "air", "version": 1, "semver": "1.2.0", "artifacts": artifacts,
        }],
    });
    peer.documents.insert(
        format!("{BASE}/schema/index.json"),
        serde_json::to_vec(&index).expect("serializes"),
    );

    let mirror = run(&dir, &peer);
    assert!(mirror.models.is_empty());
    assert!(
        mirror.flags[0].contains("does not publish model.linkml.yaml with a digest"),
        "{}",
        mirror.flags[0]
    );
}

/// The example is not one of the seven documents an endpoint has to publish, so a peer without
/// one is mirrored without one rather than refused.
#[test]
fn an_example_the_peer_does_not_publish_is_not_invented() {
    let dir = repo_dir("no-example");
    let peer = Peer::with(LINKML_BODY, |_| {});
    let mirror = run(&dir, &peer);

    assert!(mirror.flags.is_empty(), "{:?}", mirror.flags);
    let model = &mirror.models[0];
    assert_eq!(model.files.len(), 2);
    let spec: jc_core::kinds::DataModelSpec =
        serde_json::from_value(model.manifest.spec.clone()).expect("the mirror is a DataModel");
    assert_eq!(spec.artifacts.example, None);
    assert_eq!(
        spec.artifacts.context.as_deref(),
        Some("partner-air/context.jsonld")
    );
}
