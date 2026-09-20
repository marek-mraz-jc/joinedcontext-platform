//! The demo repository the plan, apply and deletion tests reconcile.
#![allow(
    dead_code,
    reason = "each test binary uses a different part of the fixture"
)]

use jcctl::loader::{RawManifest, Repository};
use std::path::{Path, PathBuf};

pub const ENDPOINT_PATH: &str = "projects/ovzdusie/spaces/ovzdusie/endpoints/public-air.yaml";

pub const ORG: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata:
  name: banskabystrica
  namespace: org
spec:
  domain: banskabystrica.sk
  locales: ["sk"]
  defaultLocale: sk
"#;

pub const PROJECT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: ovzdusie
  namespace: org
spec:
  organizationRef: banskabystrica
"#;

pub const SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
spec:
  isSandbox: false
"#;

pub const ENDPOINT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: public-air
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  enabledRepresentations: ["ngsi-ld", "geojson"]
"#;

pub fn temp_dir(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("jcctl-{test_name}-{}-{now}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp repo");
    dir
}

pub fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("relative path has a parent"))
        .expect("create parent directory");
    std::fs::write(path, body).expect("write manifest");
}

/// The four manifests of the demo story, each at the path its kind prescribes.
pub fn demo_repo(test_name: &str) -> PathBuf {
    let dir = temp_dir(test_name);
    write(&dir, "org.yaml", ORG);
    write(&dir, "projects/ovzdusie/project.yaml", PROJECT);
    write(&dir, "projects/ovzdusie/spaces/ovzdusie/space.yaml", SPACE);
    write(&dir, ENDPOINT_PATH, ENDPOINT);
    dir
}

pub fn load(dir: &Path) -> Repository {
    Repository::load(dir).expect("the fixture repository loads")
}

pub fn manifest(yaml: &str) -> RawManifest {
    serde_norway::from_str(yaml).expect("manifest parses")
}
