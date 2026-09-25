//! A whole project as Git: export to bundles, import verified by head commit (T-2640, MF-45,
//! MF-46, MF-47).

mod common;

use common::{temp_dir, write, ENDPOINT, ENDPOINT_PATH, ORG, PROJECT, SPACE};
use jc_core::kinds::{Bundle, BundleRole};
use jcctl::commands::git_bundle::{export, import, INDEX};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.org", "-C"])
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn commit(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
}

/// A project repository of two commits and a release tag, as a project lives in layout 2.
fn project_repository(test: &str) -> PathBuf {
    let dir = temp_dir(test);
    git(&dir, &["init", "-q", "-b", "main"]);
    write(&dir, ".jc/layout", "2\n");
    write(
        &dir,
        "project.yaml",
        &(PROJECT.to_owned()
            + "  version: 1.0.0\n  parameters:\n    host: { type: string, default: a.example }\n"),
    );
    write(&dir, "spaces/ovzdusie/space.yaml", SPACE);
    commit(&dir, "the project and its space");
    git(&dir, &["tag", "v1.0.0"]);
    write(&dir, "spaces/ovzdusie/endpoints/public-air.yaml", ENDPOINT);
    commit(&dir, "the public endpoint");
    dir
}

fn app_repository(test: &str) -> PathBuf {
    let dir = temp_dir(test);
    git(&dir, &["init", "-q", "-b", "main"]);
    write(&dir, "index.html", "<h1>air</h1>\n");
    commit(&dir, "the app");
    dir
}

fn index(dir: &Path) -> Bundle {
    Bundle::from_yaml(&std::fs::read_to_string(dir.join(INDEX)).expect("the index"))
        .expect("parses")
}

/// MF-45, MF-46: a project and its application travel as bundles with their whole history and
/// tags, the entry without this deployment's values, and each head checked on the way in.
#[test]
fn a_project_round_trips_with_its_history_and_equal_heads() {
    let project = project_repository("bundle-round-project");
    let app = app_repository("bundle-round-app");
    let out = temp_dir("bundle-round-out");
    let apps = BTreeMap::from([("app-air".to_owned(), app.clone())]);
    let written = export(&project, "ovzdusie", &apps, None, &out, "tester").expect("exports");

    let roles: Vec<(String, BundleRole)> = written
        .spec
        .repositories
        .iter()
        .map(|r| (r.name.clone(), r.role))
        .collect();
    assert_eq!(
        roles,
        [
            ("ovzdusie".to_owned(), BundleRole::Project),
            ("app-air".to_owned(), BundleRole::Application)
        ]
    );
    assert_eq!(
        written.spec.repositories[0].head,
        git(&project, &["rev-parse", "HEAD"])
    );
    assert_eq!(index(&out), written);

    let entry = std::fs::read_to_string(out.join("projects/ovzdusie.yaml")).expect("the entry");
    let entry = jc_core::kinds::Project::from_yaml(&entry).expect("parses");
    entry.validate().expect("valid");
    assert!(
        entry.spec.parameters.is_empty(),
        "no values of this deployment travel"
    );

    let landed = temp_dir("bundle-round-in");
    let imported = import(&out, &landed).expect("imports");
    assert_eq!(imported.repositories.len(), 2);
    for (name, path, head) in &imported.repositories {
        assert_eq!(&git(path, &["rev-parse", "HEAD"]), head, "{name}");
    }
    let air = landed.join("ovzdusie");
    assert_eq!(git(&air, &["log", "--format=%s"]).lines().count(), 2);
    assert_eq!(git(&air, &["tag"]), "v1.0.0");
    assert!(
        git(&air, &["remote"]).is_empty(),
        "the bundle is not left as a remote"
    );
    assert!(landed.join("projects/ovzdusie.yaml").is_file());
}

/// MF-46: a bundle that is not the file the index names, and a head that is not the one it
/// lists, refuse the import before anything is taken for verified.
#[test]
fn a_tampered_bundle_or_a_moved_head_is_refused() {
    let project = project_repository("bundle-tamper-project");
    let out = temp_dir("bundle-tamper-out");
    export(&project, "ovzdusie", &BTreeMap::new(), None, &out, "tester").expect("exports");

    let text = std::fs::read_to_string(out.join(INDEX)).expect("index");
    let head = index(&out).spec.repositories[0].head.clone();
    let parent = git(&project, &["rev-parse", "HEAD~1"]);
    std::fs::write(out.join(INDEX), text.replace(&head, &parent)).expect("rewrite");
    let moved = import(&out, &temp_dir("bundle-moved-in")).expect_err("a head that moved");
    assert!(moved.to_string().contains("not verified"), "{moved}");
    std::fs::write(out.join(INDEX), &text).expect("restore");

    let mut bytes = std::fs::read(out.join("ovzdusie.bundle")).expect("bundle");
    bytes.push(b'\n');
    std::fs::write(out.join("ovzdusie.bundle"), bytes).expect("tamper");
    let tampered = import(&out, &temp_dir("bundle-tampered-in")).expect_err("a changed file");
    assert!(tampered.to_string().contains("SHA-256"), "{tampered}");
}

/// MF-47: a project repository of a layout this release does not read is refused on export and
/// on import; an index whose file points out of the bundle is refused before anything is read.
#[test]
fn a_newer_layout_and_a_path_out_of_the_bundle_are_refused() {
    let project = project_repository("bundle-newer-project");
    let out = temp_dir("bundle-newer-out");
    export(&project, "ovzdusie", &BTreeMap::new(), None, &out, "tester").expect("exports");

    write(&project, ".jc/layout", "3\n");
    commit(&project, "a layout from the future");
    let refused = export(
        &project,
        "ovzdusie",
        &BTreeMap::new(),
        None,
        &temp_dir("n2"),
        "tester",
    )
    .expect_err("export of layout 3");
    assert!(refused.to_string().contains(".jc/layout"), "{refused}");

    // The same repository, bundled by hand at the newer layout, is refused on import.
    let head = git(&project, &["rev-parse", "HEAD"]);
    let bundle = out.join("ovzdusie.bundle");
    std::fs::remove_file(&bundle).expect("replace the bundle");
    git(
        &project,
        &[
            "bundle",
            "create",
            "-q",
            &bundle.to_string_lossy(),
            "HEAD",
            "--branches",
        ],
    );
    let digest = {
        use sha2::Digest;
        format!(
            "{:x}",
            sha2::Sha256::digest(std::fs::read(&bundle).expect("read"))
        )
    };
    let mut index_now = index(&out);
    index_now.spec.repositories[0].head = head;
    index_now.spec.files[0].sha256 = digest;
    std::fs::write(
        out.join(INDEX),
        serde_norway::to_string(&index_now).expect("yaml"),
    )
    .expect("w");
    let newer = import(&out, &temp_dir("bundle-newer-in")).expect_err("layout 3 on import");
    assert!(newer.to_string().contains(".jc/layout"), "{newer}");

    index_now.spec.files[0].path = "../../outside.bundle".to_owned();
    std::fs::write(
        out.join(INDEX),
        serde_norway::to_string(&index_now).expect("yaml"),
    )
    .expect("w");
    let outside = import(&out, &temp_dir("bundle-outside-in")).expect_err("a path out");
    assert!(outside.to_string().contains("files.path"), "{outside}");
}

/// MF-47: an organization repository of layout 1 is migrated on the way in, and what lands is
/// the organization repository of layout 2 and one repository per project.
#[test]
fn an_older_organization_is_migrated_on_import() {
    let org = temp_dir("bundle-older-org");
    git(&org, &["init", "-q", "-b", "main"]);
    write(&org, "org.yaml", ORG);
    write(&org, "projects/ovzdusie/project.yaml", PROJECT);
    write(&org, "projects/ovzdusie/spaces/ovzdusie/space.yaml", SPACE);
    write(&org, ENDPOINT_PATH, ENDPOINT);
    commit(&org, "the organization of layout 1");

    let out = temp_dir("bundle-older-out");
    let bundle = out.join("banskabystrica.bundle");
    git(
        &org,
        &[
            "bundle",
            "create",
            "-q",
            &bundle.to_string_lossy(),
            "HEAD",
            "--branches",
        ],
    );
    let digest = {
        use sha2::Digest;
        format!(
            "{:x}",
            sha2::Sha256::digest(std::fs::read(&bundle).expect("read"))
        )
    };
    let index = format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Bundle\nmetadata: {{ name: banskabystrica, namespace: org }}\n\
         spec:\n  exportedAt: 2026-09-22T12:00:00Z\n  exportedBy: tester\n  sourceRevision: {head}\n  items: []\n  \
         files: [ {{ path: banskabystrica.bundle, sha256: {digest} }} ]\n  \
         repositories: [ {{ name: banskabystrica, role: organization, file: banskabystrica.bundle, head: {head} }} ]\n",
        head = git(&org, &["rev-parse", "HEAD"]),
    );
    std::fs::write(out.join(INDEX), index).expect("index");

    let landed = temp_dir("bundle-older-in");
    let imported = import(&out, &landed).expect("migrated on the way in");
    let names: Vec<&str> = imported
        .repositories
        .iter()
        .map(|(n, _, _)| n.as_str())
        .collect();
    assert_eq!(names, ["banskabystrica", "ovzdusie"]);
    let migrated = &imported.repositories[0].1;
    assert_eq!(
        std::fs::read_to_string(migrated.join(".jc/layout")).expect("layout"),
        "2\n"
    );
    assert!(migrated.join("projects/ovzdusie.yaml").is_file());
}

/// An organization model, as the organization checkout keeps it (DM-75).
const ORGANIZATION_MODEL: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: stations
  namespace: org
spec:
  linkml: stations.linkml.yaml
  version: 1.3.0
  lifecycle: published
  classes: ["Station"]
"#;
const ORGANIZATION_SOURCE: &str = "id: https://example.org/stations\nname: stations\n";

/// The space's own model, importing the organization's (DM-76).
const SPACE_MODEL: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: parking
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  linkml: parking.linkml.yaml
  version: 1.0.0
  lifecycle: published
  classes: ["ParkingSpot"]
"#;
const SPACE_SOURCE: &str =
    "id: https://banskabystrica.sk/parking\nname: parking\nimports: [linkml:types, org.stations.v1]\n";

/// MF-49: a project importing an organization model carries a copy of it, listed in the index
/// with its version, the SHA-256 of its source and its origin; it holds the schema files alone,
/// and without the organization checkout the export is refused naming the import.
#[test]
fn an_imported_organization_model_travels_with_the_project() {
    let project = project_repository("bundle-model-project");
    write(
        &project,
        "spaces/ovzdusie/datamodels/parking.yaml",
        SPACE_MODEL,
    );
    write(
        &project,
        "spaces/ovzdusie/datamodels/parking.linkml.yaml",
        SPACE_SOURCE,
    );
    commit(&project, "a model importing the organization's stations");
    let organization = common::demo_repo("bundle-model-organization");
    write(
        &organization,
        "datamodels/stations/stations.yaml",
        ORGANIZATION_MODEL,
    );
    write(
        &organization,
        "datamodels/stations/stations.linkml.yaml",
        ORGANIZATION_SOURCE,
    );

    let refused = export(
        &project,
        "ovzdusie",
        &BTreeMap::new(),
        None,
        &temp_dir("bundle-model-refused"),
        "tester",
    )
    .expect_err("an import with nothing to carry it from");
    assert!(
        refused.to_string().contains("org.stations.v1")
            && refused.to_string().contains("--org-dir"),
        "{refused}"
    );

    let out = temp_dir("bundle-model-out");
    let written = export(
        &project,
        "ovzdusie",
        &BTreeMap::new(),
        Some(&organization),
        &out,
        "tester",
    )
    .expect("exports");
    let [model] = written.spec.models.as_slice() else {
        panic!("one carried model: {:?}", written.spec.models);
    };
    assert_eq!(model.name, "stations");
    assert_eq!(model.version.to_string(), "1.3.0");
    assert_eq!(model.origin.organization, "banskabystrica");
    assert_eq!(model.origin.name, "stations");
    assert_eq!(model.file, "models/stations.v1.linkml.yaml");
    assert_eq!(
        std::fs::read_to_string(out.join(&model.file)).expect("the source"),
        ORGANIZATION_SOURCE
    );
    assert_eq!(
        std::fs::read_to_string(out.join(&model.manifest)).expect("the manifest"),
        ORGANIZATION_MODEL
    );
    use sha2::Digest as _;
    assert_eq!(
        model.sha256,
        format!("{:x}", sha2::Sha256::digest(ORGANIZATION_SOURCE))
    );
    assert_eq!(index(&out), written);

    // Schema files only: nothing else of the organization checkout travels.
    let mut carried: Vec<String> = std::fs::read_dir(out.join("models"))
        .expect("models/")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    carried.sort();
    assert_eq!(carried, ["stations.v1.linkml.yaml", "stations.v1.yaml"]);
    assert!(!out.join("org.yaml").exists());

    // And it comes along on import, verified like every file of the index.
    let landed = temp_dir("bundle-model-in");
    import(&out, &landed).expect("imports");
    assert!(landed.join("models/stations.v1.linkml.yaml").is_file());
}
