use jcctl::loader::{LoadError, RawMetadata, Repository, ResourceId};
use serde_json::json;
use std::path::{Path, PathBuf};

fn unique_temp_dir(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "jcctl-load-{test_name}-{}-{now}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn loads_valid_repository_with_expected_count_and_ids() {
    let dir = unique_temp_dir("valid-repo");

    std::fs::write(
        dir.join("org.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata:
  name: my-city
  namespace: org
spec:
  domain: banskabystrica.sk
  locales: ["sk", "en"]
  defaultLocale: sk
"#,
    )
    .unwrap();

    let p1_dir = dir.join("projects/doprava");
    std::fs::create_dir_all(&p1_dir).unwrap();
    std::fs::write(
        p1_dir.join("project.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: doprava
  namespace: org
spec:
  organizationRef: my-city
"#,
    )
    .unwrap();

    let p2_dir = dir.join("projects/odpady");
    std::fs::create_dir_all(&p2_dir).unwrap();
    std::fs::write(
        p2_dir.join("project.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: odpady
  namespace: org
spec:
  organizationRef: my-city
"#,
    )
    .unwrap();

    let space_dir = p1_dir.join("spaces/senzory");
    std::fs::create_dir_all(&space_dir).unwrap();
    std::fs::write(
        space_dir.join("space.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: senzory
  namespace: doprava
spec:
  isSandbox: false
"#,
    )
    .unwrap();

    let ep_dir = space_dir.join("endpoints");
    std::fs::create_dir_all(&ep_dir).unwrap();
    std::fs::write(
        ep_dir.join("public-stream.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: public-stream
  namespace: doprava
spec:
  contextSpaceRef: senzory
  slug: abcdefghijklmnopqrstuvwxyz
  audience: public
  enabledRepresentations: ["ngsi-ld"]
"#,
    )
    .unwrap();

    let pol_dir = space_dir.join("policies");
    std::fs::create_dir_all(&pol_dir).unwrap();
    std::fs::write(
        pol_dir.join("read-access.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: read-access
  namespace: doprava
spec:
  contextSpaceRef: senzory
  assigner: did:web:banskabystrica.sk
  assignee:
    kind: role
    id: viewer
  operations: ["retrieveEntity"]
"#,
    )
    .unwrap();

    let repo = Repository::load(&dir).expect("valid repository loads");
    assert_eq!(repo.len(), 6);
    assert!(!repo.is_empty());
    assert_eq!(repo.root(), &dir);

    let org_id = ResourceId::new(
        "joinedcontext.com",
        "Organization",
        Some("org".into()),
        "my-city",
    );
    assert!(repo.get(&org_id).is_some());

    let p1_id = ResourceId::new(
        "joinedcontext.com",
        "Project",
        Some("org".into()),
        "doprava",
    );
    assert!(repo.get(&p1_id).is_some());

    let space_id = ResourceId::new(
        "joinedcontext.com",
        "ContextSpace",
        Some("doprava".into()),
        "senzory",
    );
    assert!(repo.get(&space_id).is_some());

    let ep_id = ResourceId::new(
        "joinedcontext.com",
        "Endpoint",
        Some("doprava".into()),
        "public-stream",
    );
    assert!(repo.get(&ep_id).is_some());

    assert!(repo.misplaced().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn multi_document_file_yields_expected_line_and_document_numbers() {
    let dir = unique_temp_dir("multi-doc");

    let multi_content = r#"---
apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: proj-a
  namespace: org
spec:
  organizationRef: my-city
---

---
apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: proj-b
  namespace: org
spec:
  organizationRef: my-city
---
"#;
    std::fs::write(dir.join("projects.yaml"), multi_content).unwrap();

    let repo = Repository::load(&dir).expect("loads multi-document manifest");
    assert_eq!(repo.len(), 2);

    let id_a = ResourceId::new("joinedcontext.com", "Project", Some("org".into()), "proj-a");
    let res_a = repo.get(&id_a).expect("proj-a exists");
    assert_eq!(res_a.document, 1);
    assert_eq!(res_a.line, 1);

    let id_b = ResourceId::new("joinedcontext.com", "Project", Some("org".into()), "proj-b");
    let res_b = repo.get(&id_b).expect("proj-b exists");
    assert_eq!(res_b.document, 3);
    assert_eq!(res_b.line, 11);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn skips_native_linkml_bento_git_and_markdown_files() {
    let dir = unique_temp_dir("skip-files");

    std::fs::write(
        dir.join("org.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata:
  name: test-org
  namespace: org
spec:
  domain: banskabystrica.sk
  locales: ["sk"]
  defaultLocale: sk
"#,
    )
    .unwrap();

    std::fs::write(dir.join("README.md"), "# Test Readme\n").unwrap();
    std::fs::write(dir.join("bento.yaml"), "input: {}\noutput: {}\n").unwrap();
    std::fs::write(dir.join("model.linkml.yaml"), "id: urn:test\n").unwrap();

    let git_dir = dir.join(".git");
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(git_dir.join("config.yaml"), "invalid yaml\n").unwrap();

    let repo = Repository::load(&dir).expect("loads repository skipping ignored files");
    assert_eq!(repo.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn duplicate_identity_across_files_returns_duplicate_identity_error() {
    let dir = unique_temp_dir("dup-id");

    std::fs::write(
        dir.join("file1.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: shared-proj
  namespace: org
spec:
  organizationRef: org-1
"#,
    )
    .unwrap();

    std::fs::write(
        dir.join("file2.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: shared-proj
  namespace: org
spec:
  organizationRef: org-2
"#,
    )
    .unwrap();

    let err = Repository::load(&dir).expect_err("duplicate identity must fail");
    match err {
        LoadError::DuplicateIdentity { id, first, second } => {
            assert_eq!(id.name, "shared-proj");
            assert_eq!(first, Path::new("file1.yaml"));
            assert_eq!(second, Path::new("file2.yaml"));
        }
        other => panic!("expected DuplicateIdentity, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_kind_and_wrong_api_version_fail() {
    let dir = unique_temp_dir("invalid-manifest");

    let unknown_file = dir.join("unknown.yaml");
    std::fs::write(
        &unknown_file,
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: UnknownKind
metadata:
  name: foo
  namespace: org
spec: {}
"#,
    )
    .unwrap();

    let err_kind = Repository::load(&dir).expect_err("unknown kind must fail");
    assert!(matches!(err_kind, LoadError::UnknownKind { ref kind, .. } if kind == "UnknownKind"));

    std::fs::remove_file(unknown_file).unwrap();

    std::fs::write(
        dir.join("wrong-api.yaml"),
        r#"apiVersion: joinedcontext.com/v2beta1
kind: Organization
metadata:
  name: bar
  namespace: org
spec: {}
"#,
    )
    .unwrap();

    let err_api = Repository::load(&dir).expect_err("wrong apiVersion must fail");
    assert!(
        matches!(err_api, LoadError::ApiVersion { ref got, .. } if got == "joinedcontext.com/v2beta1")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn symlink_pointing_outside_repository_root_is_refused() {
    let root = unique_temp_dir("symlink-root");
    let outside = unique_temp_dir("symlink-outside");

    let target = outside.join("outside.yaml");
    std::fs::write(
        &target,
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: outside-proj
  namespace: org
spec:
  organizationRef: outside-org
"#,
    )
    .unwrap();

    let link = root.join("link.yaml");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let err = Repository::load(&root).expect_err("symlink escaping repository must fail");
    assert!(matches!(err, LoadError::PathEscapesRepository { .. }));

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn misplaced_reports_resource_at_wrong_path_and_is_empty_when_correct() {
    let dir = unique_temp_dir("misplaced");

    let wrong_dir = dir.join("projects/doprava/wrong-folder");
    std::fs::create_dir_all(&wrong_dir).unwrap();
    std::fs::write(
        wrong_dir.join("public-stream.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: public-stream
  namespace: doprava
spec:
  contextSpaceRef: senzory
  slug: abcdefghijklmnopqrstuvwxyz
  audience: public
  enabledRepresentations: ["ngsi-ld"]
"#,
    )
    .unwrap();

    let repo = Repository::load(&dir).expect("loads misplaced resource");
    let misplaced = repo.misplaced();
    assert_eq!(misplaced.len(), 1);

    let (id, actual, expected) = &misplaced[0];
    assert_eq!(id.name, "public-stream");
    assert_eq!(
        actual,
        &PathBuf::from("projects/doprava/wrong-folder/public-stream.yaml")
    );
    assert_eq!(
        expected,
        "projects/doprava/spaces/senzory/endpoints/public-stream.yaml"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A `linkml:` payload is an embedded YAML document; its indented `---` lines belong to
/// the block scalar and must not split the manifest into fragments (MF-05).
#[test]
fn indented_separator_inside_a_block_scalar_does_not_split_the_document() {
    let dir = unique_temp_dir("block-scalar");

    std::fs::write(
        dir.join("model.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: traffic
  namespace: doprava
spec:
  contextSpaceRef: senzory
  linkml: |
    id: https://banskabystrica.sk/schemas/traffic
    ---
    name: traffic
"#,
    )
    .unwrap();

    let repo = Repository::load(&dir).expect("block scalar loads as one document");
    assert_eq!(repo.len(), 1);

    let id = ResourceId::new(
        "joinedcontext.com",
        "DataModel",
        Some("doprava".into()),
        "traffic",
    );
    let linkml = repo.get(&id).expect("the data model").manifest.spec["linkml"]
        .as_str()
        .expect("linkml is a string");
    assert!(linkml.contains("---"), "the separator stays in the payload");

    let _ = std::fs::remove_dir_all(&dir);
}

/// MF-06 and MF-07: `contextSpaceRef` is written two ways and both name the same space.
/// Reading only the bare-label form put every `Policy` at `.../spaces//policies/…`, so
/// `validate` reported a correctly placed manifest as misplaced.
#[test]
fn a_typed_context_space_reference_resolves_the_space_placeholder() {
    let dir = unique_temp_dir("typed-space-ref");
    let policies = dir.join("projects/ovzdusie/spaces/ovzdusie/policies");
    std::fs::create_dir_all(&policies).unwrap();

    std::fs::write(
        policies.parent().unwrap().join("space.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
spec:
  isSandbox: false
"#,
    )
    .unwrap();
    std::fs::write(
        policies.join("public-air-quality.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: public-air-quality
  namespace: ovzdusie
spec:
  contextSpaceRef:
    kind: ContextSpace
    name: ovzdusie
  assigner: "did:web:banskabystrica.sk"
  assignee: { kind: role, id: public }
  operations: [queryEntity]
  information:
    - entities:
        - type: AirQualityObserved
"#,
    )
    .unwrap();

    let repo = Repository::load(&dir).expect("the repository loads");
    assert!(
        repo.misplaced().is_empty(),
        "a typed reference names its space too: {:?}",
        repo.misplaced()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// T-0285, CC-08: a symlinked file whose target is inside the root is a file and loads;
/// the containment check is unchanged and still refuses a target outside.
#[test]
fn a_symlinked_file_inside_the_root_loads() {
    let root = unique_temp_dir("symlink-inside");
    // `.real/` is a dot-directory so the manifest is reachable once, through the link only —
    // the way a ConfigMap keeps its versions under `..data`.
    let real = root.join(".real");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(
        real.join("org.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata:
  name: my-city
  namespace: org
spec:
  domain: banskabystrica.sk
  locales: ["sk"]
  defaultLocale: sk
"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(real.join("org.yaml"), root.join("org.yaml")).unwrap();

    let repo = Repository::load(&root).expect("a link inside the root loads");
    assert_eq!(repo.len(), 1, "the linked manifest loads exactly once");
    assert!(repo.misplaced().is_empty(), "{:?}", repo.misplaced());

    let _ = std::fs::remove_dir_all(&root);
}

/// T-0285: the exact layout of a Kubernetes ConfigMap or Secret volume — every file a
/// symlink into `..data/`, itself a symlink to a dotted version directory — loads every
/// manifest once. A test with plain files only cannot fail on this bug.
#[test]
fn a_configmap_style_volume_of_symlinks_loads_every_manifest_once() {
    let root = unique_temp_dir("symlink-configmap");
    let version = root.join("..2026_09_06_17_00_00.000000000");
    std::fs::create_dir_all(version.join("projects/doprava")).unwrap();
    std::fs::write(
        version.join("org.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata:
  name: my-city
  namespace: org
spec:
  domain: banskabystrica.sk
  locales: ["sk"]
  defaultLocale: sk
"#,
    )
    .unwrap();
    std::fs::write(
        version.join("projects/doprava/project.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: doprava
  namespace: org
spec:
  organizationRef: my-city
"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(&version, root.join("..data")).unwrap();
    std::os::unix::fs::symlink("..data/org.yaml", root.join("org.yaml")).unwrap();
    // A directory symlink inside the root is followed, never silently ignored.
    std::os::unix::fs::symlink("..data/projects", root.join("projects")).unwrap();

    let repo = Repository::load(&root).expect("the ConfigMap layout loads");
    assert_eq!(repo.len(), 2, "one Organization and one Project, each once");
    assert!(repo
        .get(&ResourceId::new(
            "joinedcontext.com",
            "Project",
            Some("org".to_owned()),
            "doprava"
        ))
        .is_some());
    assert!(
        repo.misplaced().is_empty(),
        "the path is the link's path, which is where the manifest belongs: {:?}",
        repo.misplaced()
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// T-0285: a dangling link is skipped with a warning; it never fails the load and never
/// counts as a file, and the manifests beside it still load.
#[test]
fn a_dangling_link_is_skipped_and_the_rest_loads() {
    let root = unique_temp_dir("symlink-dangling");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("org.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata:
  name: my-city
  namespace: org
spec:
  domain: banskabystrica.sk
  locales: ["sk"]
  defaultLocale: sk
"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(root.join("missing.yaml"), root.join("gone.yaml")).unwrap();

    let repo = Repository::load(&root).expect("a dangling link does not fail the load");
    assert_eq!(repo.len(), 1);

    let _ = std::fs::remove_dir_all(&root);
}

/// T-0285, CC-08: a directory link whose target leaves the root is refused before the walk
/// descends into it.
#[test]
fn a_directory_link_outside_the_root_is_refused() {
    let root = unique_temp_dir("symlink-dir-root");
    let outside = unique_temp_dir("symlink-dir-outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("x.yaml"), "").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("projects")).unwrap();

    let err = Repository::load(&root).expect_err("a directory link out of the root must fail");
    assert!(
        matches!(err, LoadError::PathEscapesRepository { .. }),
        "{err:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

/// T-0451: a configuration repository holding a `portal/forms/*.uischema.yaml` loads.
///
/// Before `kind: UiSchema` was in the registry this failed with [`LoadError::UnknownKind`] —
/// and failed the **whole repository**, not the one file, so the first form manifest anybody
/// committed broke `jcctl validate` for everything beside it (UI-02, MF-06).
#[test]
fn a_repository_holding_a_uischema_loads_and_the_manifest_is_at_its_own_path() {
    let dir = unique_temp_dir("uischema");

    std::fs::write(
        dir.join("org.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata:
  name: my-city
  namespace: org
spec:
  domain: banskabystrica.sk
  locales: ["sk", "en"]
  defaultLocale: sk
"#,
    )
    .unwrap();

    let forms = dir.join("portal/forms");
    std::fs::create_dir_all(&forms).unwrap();
    std::fs::write(
        forms.join("endpoint.uischema.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: UiSchema
metadata:
  name: endpoint
  namespace: org
spec:
  for: Endpoint
  order: [name, slug]
  groups:
    - title: { sk: "Základ", en: "Basics" }
      fields: [name, slug]
  fields:
    slug:
      widget: text
      columns: 6
    commitMessage:
      advanced: true
"#,
    )
    .unwrap();

    let repo = Repository::load(&dir).expect("a repository with a form manifest loads");
    let id = ResourceId::new(
        "joinedcontext.com",
        "UiSchema",
        Some("org".into()),
        "endpoint",
    );
    assert!(
        repo.get(&id).is_some(),
        "the form manifest is in the index like any other kind"
    );
    // The Portal reads `portal/forms/{name}.uischema.yaml`; if the registry disagreed about
    // the path the file would be reported misplaced and the two ends would drift apart.
    assert!(
        repo.misplaced().is_empty(),
        "the documented path is the registry's path: {:?}",
        repo.misplaced()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// UI-50: the legacy `{locale: text}` title collapses to one string on the way out: `en`,
/// then the first non-empty value; a map of nothing goes, a malformed map stays for
/// validation to refuse, and a plain string is left alone.
#[test]
fn a_legacy_language_map_collapses_to_one_string() {
    let collapsed = |title: serde_json::Value| {
        let mut metadata: RawMetadata =
            serde_json::from_value(json!({ "name": "air", "title": title })).expect("metadata");
        metadata.collapse_language_maps();
        metadata.rest.get("title").cloned()
    };
    assert_eq!(
        collapsed(json!({ "sk": "Ovzdušie", "en": "Air" })),
        Some(json!("Air"))
    );
    assert_eq!(
        collapsed(json!({ "cs": "", "en": "", "sk": "Ovzdušie" })),
        Some(json!("Ovzdušie"))
    );
    assert_eq!(collapsed(json!({ "en": "" })), None);
    assert_eq!(
        collapsed(json!({ "en": { "nested": true } })),
        Some(json!({ "en": { "nested": true } }))
    );
    assert_eq!(collapsed(json!("Air")), Some(json!("Air")));
}

/// CC-73, CC-74: one repository, every environment. The same manifests render with the overlay
/// `JC_ENVIRONMENT` names, so a URN carries `{orgDomain}` and never a domain of its own.
fn repository_with_overlays(test_name: &str) -> PathBuf {
    let dir = unique_temp_dir(test_name);
    std::fs::write(
        dir.join("org.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata: { name: my-city, namespace: org }
spec:
  domain: banskabystrica.sk
  locales: ["sk", "en"]
  defaultLocale: sk
"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("environments")).unwrap();
    std::fs::write(
        dir.join("environments/dev.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Environment
metadata: { name: dev, namespace: org }
spec:
  orgDomain: dev.banskabystrica.sk
  hosts: { portal: portal.dev.bb.example }
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("environments/staging.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Environment
metadata: { name: staging, namespace: org }
spec:
  orgDomain: staging.banskabystrica.sk
"#,
    )
    .unwrap();
    let project = dir.join("projects/doprava");
    std::fs::create_dir_all(project.join("spaces/mhd")).unwrap();
    std::fs::write(
        project.join("project.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata: { name: doprava, namespace: org }
spec:
  organizationRef: my-city
"#,
    )
    .unwrap();
    std::fs::write(
        project.join("spaces/mhd/space.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: mhd
  namespace: doprava
spec:
  isSandbox: false
"#,
    )
    .unwrap();
    // A URN and a query string, the two places a domain reaches a manifest.
    std::fs::create_dir_all(project.join("pipelines/stops")).unwrap();
    std::fs::write(
        project.join("pipelines/stops/pipeline.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Pipeline
metadata: { name: stops, namespace: doprava }
spec:
  class: resident
  source:
    endpointRef: { kind: Endpoint, name: stops }
    query: { q: 'owner=="{orgDomain}"' }
  compute: { kind: bloblang, bloblang: "root = this" }
  targetEndpoint: "urn:ngsi-ld:Endpoint:{orgDomain}:mhd:stops"
"#,
    )
    .unwrap();
    dir
}

#[test]
fn the_same_repository_renders_the_domain_of_the_environment_it_is_loaded_for() {
    let dir = repository_with_overlays("overlays");
    let pipeline = ResourceId::new(
        "joinedcontext.com",
        "Pipeline",
        Some("doprava".into()),
        "stops",
    );

    let seed = |environment: Option<&str>| {
        let repo = Repository::load_for(&dir, environment).expect("loads");
        let rendered = repo
            .get(&pipeline)
            .expect("the pipeline")
            .manifest
            .spec
            .get("targetEndpoint")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        (repo.org_domain().map(str::to_owned), rendered)
    };

    // Without an overlay: the Organization's own domain, which is what happened until now.
    let (domain, rendered) = seed(None);
    assert_eq!(domain.as_deref(), Some("banskabystrica.sk"));
    assert_eq!(rendered, "urn:ngsi-ld:Endpoint:banskabystrica.sk:mhd:stops");

    let (dev, dev_urn) = seed(Some("dev"));
    let (staging, staging_urn) = seed(Some("staging"));
    assert_eq!(dev.as_deref(), Some("dev.banskabystrica.sk"));
    assert_eq!(staging.as_deref(), Some("staging.banskabystrica.sk"));
    // The same manifest, two environments, no diff of its own.
    assert_eq!(
        dev_urn,
        "urn:ngsi-ld:Endpoint:dev.banskabystrica.sk:mhd:stops"
    );
    assert_eq!(
        staging_urn,
        "urn:ngsi-ld:Endpoint:staging.banskabystrica.sk:mhd:stops"
    );

    // The hosts come with the overlay, so the reconciler reads them from the load.
    let repo = Repository::load_for(&dir, Some("dev")).expect("loads");
    assert_eq!(
        repo.hosts().get("portal").map(String::as_str),
        Some("portal.dev.bb.example")
    );
    assert_eq!(repo.environment(), Some("dev"));
}

#[test]
fn the_placeholder_is_rendered_inside_a_urn_and_inside_a_query() {
    let dir = unique_temp_dir("placeholder");
    std::fs::write(
        dir.join("org.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata: { name: my-city, namespace: org }
spec:
  domain: banskabystrica.sk
  locales: ["sk"]
  defaultLocale: sk
"#,
    )
    .unwrap();
    let project = dir.join("projects/doprava");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("project.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata: { name: doprava, namespace: org }
spec:
  organizationRef: my-city
"#,
    )
    .unwrap();
    std::fs::write(
        project.join("pipeline.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Pipeline
metadata: { name: stops, namespace: doprava }
spec:
  class: resident
  source:
    endpointRef: { kind: Endpoint, name: stops }
    query: 'q=owner=="{orgDomain}"'
  compute: { kind: bloblang, bloblang: "root = this" }
  targetEndpoint: "urn:ngsi-ld:Endpoint:{orgDomain}:mhd:stops"
"#,
    )
    .unwrap();

    let repo = Repository::load_for(&dir, None).expect("loads");
    let pipeline = repo
        .get(&ResourceId::new(
            "joinedcontext.com",
            "Pipeline",
            Some("doprava".into()),
            "stops",
        ))
        .expect("the pipeline");
    assert_eq!(
        pipeline.manifest.spec["targetEndpoint"],
        json!("urn:ngsi-ld:Endpoint:banskabystrica.sk:mhd:stops")
    );
    assert_eq!(
        pipeline.manifest.spec["source"]["query"],
        json!("q=owner==\"banskabystrica.sk\"")
    );
}

#[test]
fn an_overlay_with_an_unknown_field_is_refused_and_so_is_one_that_is_not_there() {
    let dir = unique_temp_dir("bad-overlay");
    std::fs::create_dir_all(dir.join("environments")).unwrap();
    std::fs::write(
        dir.join("environments/dev.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Environment
metadata: { name: dev, namespace: org }
spec:
  orgDomain: dev.banskabystrica.sk
  secrets: { backend: openbao, value: hunter2 }
"#,
    )
    .unwrap();

    let refused = Repository::load_for(&dir, Some("dev")).expect_err("a secret value");
    assert!(matches!(refused, LoadError::Overlay { .. }), "{refused:?}");
    assert!(format!("{refused}").contains("value"), "{refused}");

    let missing = Repository::load_for(&dir, Some("production")).expect_err("no such overlay");
    assert!(
        matches!(missing, LoadError::NoSuchEnvironment { .. }),
        "{missing:?}"
    );
}

/// CC-74: a manifest that writes the organization's domain out is named while the repository is
/// migrated, and the overlay itself — the one place a domain belongs — is not.
#[test]
fn a_manifest_that_writes_the_domain_out_is_reported_and_the_overlay_is_not() {
    let dir = repository_with_overlays("literal-domain");
    let project = dir.join("projects/doprava");
    std::fs::write(
        project.join("pipelines/stops/pipeline.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Pipeline
metadata: { name: stops, namespace: doprava }
spec:
  class: resident
  source:
    endpointRef: { kind: Endpoint, name: stops }
    query: { q: 'owner=="dev.banskabystrica.sk"' }
  compute: { kind: bloblang, bloblang: "root = this" }
  targetEndpoint: "urn:ngsi-ld:Endpoint:{orgDomain}:mhd:stops"
"#,
    )
    .unwrap();

    let repo = Repository::load_for(&dir, Some("dev")).expect("loads");
    let written: Vec<&str> = repo
        .literal_domains()
        .iter()
        .map(|(_, _, text)| text.as_str())
        .collect();
    assert_eq!(written, ["owner==\"dev.banskabystrica.sk\""], "{written:?}");

    let report = jcctl::commands::validate::run(&dir);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.message.contains("{orgDomain}")),
        "{:?}",
        report.warnings
    );
    // A warning is not a refusal while the repository is being migrated.
    assert!(report.findings.is_empty(), "{:?}", report.findings);
}

/// AP-13a: the build lane writes `status.build` into the manifest, so the loader carries it —
/// every other part of `status` is computed and never stored (MF-04).
#[test]
fn the_build_status_of_an_app_survives_the_load() {
    let dir = unique_temp_dir("app-build-status");
    std::fs::write(
        dir.join("org.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata: { name: my-city, namespace: org }
spec:
  domain: banskabystrica.sk
  locales: ["sk"]
  defaultLocale: sk
"#,
    )
    .unwrap();
    let app = dir.join("projects/doprava/apps/tabule");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        dir.join("projects/doprava/project.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata: { name: doprava, namespace: org }
spec:
  organizationRef: my-city
"#,
    )
    .unwrap();
    std::fs::write(
        app.join("app.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: App
metadata: { name: tabule, namespace: doprava }
spec:
  kind: static
  source: { path: ./src }
  visibility: public
  dataNeeds:
    - contextSpaceRef: { kind: ContextSpace, name: mhd }
      types: [Stop]
      operations: [queryEntity]
status:
  build:
    digest: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    commit: 8c56954a1f0e
    sdkVersion: "0.4.1"
    builtAt: "2026-09-17T06:00:00Z"
"#,
    )
    .unwrap();

    let repo = Repository::load(&dir).expect("loads");
    let manifest = &repo
        .get(&ResourceId::new(
            "joinedcontext.com",
            "App",
            Some("doprava".into()),
            "tabule",
        ))
        .expect("the app")
        .manifest;
    let build = manifest
        .status
        .as_ref()
        .and_then(|status| status.get("build"))
        .expect("the build the lane wrote");
    assert_eq!(build["commit"], "8c56954a1f0e");
    assert_eq!(build["sdkVersion"], "0.4.1");
}

/// DS-09, T-2355: two `DataAgreement` manifests claiming one `spec.agreementId` are a finding,
/// and both are named.
///
/// The gateway serves neither of them, because keeping one would let whoever may write a manifest
/// in any project take another project's agreement id by choosing a namespace that sorts after it.
/// Both parties then lose their transfer tokens with nothing to read anywhere, which is why
/// `jcctl validate` has to say which manifests collided rather than leave an operator to find it.
#[test]
fn two_agreements_claiming_one_id_are_a_finding() {
    let dir = unique_temp_dir("contested-agreement-id");
    std::fs::write(
        dir.join("org.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata: { name: my-city, namespace: org }
spec:
  domain: banskabystrica.sk
  locales: ["sk"]
  defaultLocale: sk
"#,
    )
    .unwrap();

    let agreement = |project: &str, name: &str, id: &str| {
        format!(
            r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataAgreement
metadata: {{ name: {name}, namespace: {project} }}
spec:
  role: provider
  offerRef: {{ kind: DataOffer, name: air-quality }}
  remoteParticipant: did:web:helsinki.fi
  agreementId: "{id}"
  state: finalized
  validity: {{ from: 2026-01-01T00:00:00Z, to: 2027-01-01T00:00:00Z }}
"#
        )
    };
    for project in ["aaa", "zzz"] {
        let folder = dir.join("projects").join(project).join("dataspace");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("air-quality.yaml"),
            agreement(project, "air-quality", "urn:uuid:9a1f-air-quality"),
        )
        .unwrap();
    }
    let alone = dir.join("projects/aaa/dataspace");
    std::fs::write(
        alone.join("noise.yaml"),
        agreement("aaa", "noise", "urn:uuid:0000-noise"),
    )
    .unwrap();

    let repo = Repository::load(&dir).expect("loads");
    let contested = repo.contested_agreement_ids();
    assert_eq!(
        contested.len(),
        2,
        "one entry per claiming manifest, and the uncontested one is not among them: {contested:?}"
    );
    for (id, _, why) in contested {
        assert_eq!(id.name, "air-quality");
        assert!(
            why.contains("aaa/air-quality") && why.contains("zzz/air-quality"),
            "the finding names every claimant: {why}"
        );
        assert!(
            why.contains("urn:uuid:9a1f-air-quality"),
            "and the id they claim: {why}"
        );
    }
    assert!(
        !contested.iter().any(|(id, _, _)| id.name == "noise"),
        "an agreement nobody else claims is not contested"
    );

    let report = jcctl::commands::validate::run(&dir);
    let named: Vec<&str> = report
        .findings
        .iter()
        .map(|finding| finding.message.as_str())
        .filter(|message| message.contains("agreementId"))
        .collect();
    assert_eq!(named.len(), 2, "validate refuses the repository: {named:?}");
}
