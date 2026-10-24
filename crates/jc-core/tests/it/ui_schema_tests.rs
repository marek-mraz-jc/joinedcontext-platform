//! T-0451: `kind: UiSchema`, the manifest that arranges another kind's form (UI-02, MF-06, MF-09).

use jc_core::error::Error;
use jc_core::kinds::UiSchema;
use jc_core::registry;

fn parse(yaml: &str) -> Result<UiSchema, Error> {
    let parsed: UiSchema = serde_norway::from_str(yaml).map_err(|e| Error::Parse(e.to_string()))?;
    parsed.validate()?;
    Ok(parsed)
}

/// The example of `Architecture/09-portal.md#the-kind-uischema-manifest-ui-02`, verbatim.
const DOCUMENTED: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: UiSchema
metadata:
  name: endpoint
  namespace: org
spec:
  for: Endpoint
  order: [name, slug, audience, enabledRepresentations, caching]
  groups:
    - title: Basics
      fields: [name, slug]
    - title: Access
      description: Who may read
      fields: [audience, enabledRepresentations]
  fields:
    slug:
      widget: text
      help: The unguessable address
      placeholder: "26 characters"
      columns: 6
      readOnly: false
      advanced: false
    enabledRepresentations:
      widget: checkboxes
    commitMessage:
      advanced: true
"#;

fn documented_with(spec: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: UiSchema\nmetadata:\n  name: endpoint\n  \
         namespace: org\nspec:\n{spec}"
    )
}

#[test]
fn the_documented_example_parses_and_validates() {
    let manifest = parse(DOCUMENTED).expect("the documented example is the contract");
    assert_eq!(manifest.spec.for_kind, "Endpoint");
    assert_eq!(
        manifest.spec.order.first().map(String::as_str),
        Some("name")
    );
    assert_eq!(manifest.spec.groups.len(), 2);
    assert_eq!(
        manifest.spec.groups[1].fields,
        ["audience", "enabledRepresentations"]
    );
    assert!(manifest.spec.groups[0].description.is_none());
    let slug = &manifest.spec.fields["slug"];
    assert_eq!(slug.widget.as_deref(), Some("text"));
    assert_eq!(slug.placeholder.as_deref(), Some("26 characters"));
    assert_eq!(slug.columns, Some(6));
    // CC-29: the Git mechanics are declared like any other field and simply not on the
    // default form.
    assert_eq!(manifest.spec.fields["commitMessage"].advanced, Some(true));
}

/// UI-50: a legend or help written as the legacy language map still reads, and resolves to
/// the string a plain manifest would have written.
#[test]
fn a_legacy_language_map_label_still_reads() {
    let manifest = parse(&documented_with(
        "  for: Endpoint\n  groups:\n    - title: { sk: Základ, en: Basics }\n      fields: [name]\n  \
         fields:\n    slug:\n      help: { sk: Adresa }\n",
    ))
    .expect("the legacy map form reads through v0.9");
    assert_eq!(manifest.spec.groups[0].title.resolve(&[], "en"), "Basics");
    assert_eq!(
        manifest.spec.fields["slug"]
            .help
            .as_ref()
            .map(|help| help.resolve(&[], "en")),
        Some("Adresa")
    );
}

#[test]
fn a_field_the_manifest_does_not_know_is_refused_rather_than_ignored() {
    // A mistyped key silently ignored is a form that renders wrong with no message anywhere.
    let yaml = documented_with("  for: Endpoint\n  fields:\n    slug:\n      widgets: text\n");
    assert!(matches!(parse(&yaml), Err(Error::Parse(_))));
}

#[test]
fn a_form_for_a_kind_this_platform_does_not_have_is_refused() {
    let yaml = documented_with("  for: NoSuchKind\n");
    let Err(Error::Name { field, value, .. }) = parse(&yaml) else {
        panic!("a form for a kind that does not exist arranges nothing");
    };
    assert_eq!(field, "spec.for");
    assert_eq!(value, "NoSuchKind");
}

#[test]
fn a_column_count_outside_the_twelve_of_a_row_is_refused() {
    for columns in ["13", "0"] {
        let yaml = documented_with(&format!(
            "  for: Endpoint\n  fields:\n    slug:\n      columns: {columns}\n"
        ));
        let Err(Error::Name { field, .. }) = parse(&yaml) else {
            panic!("columns: {columns} is not a twelfth of a row");
        };
        assert_eq!(field, "spec.fields.columns");
    }
    for columns in ["1", "12"] {
        let yaml = documented_with(&format!(
            "  for: Endpoint\n  fields:\n    slug:\n      columns: {columns}\n"
        ));
        assert!(parse(&yaml).is_ok(), "columns: {columns} is inside the row");
    }
}

#[test]
fn a_field_named_by_two_groups_is_refused() {
    // Drawn twice or dropped, depending on which fieldset the renderer reaches first.
    let yaml = documented_with(
        "  for: Endpoint\n  groups:\n    - title: { en: One }\n      fields: [name, slug]\n    \
         - title: { en: Two }\n      fields: [slug]\n",
    );
    let Err(Error::Name { field, value, .. }) = parse(&yaml) else {
        panic!("a field belongs to one group");
    };
    assert_eq!(field, "spec.groups.fields");
    assert_eq!(value, "slug");
}

#[test]
fn the_name_is_the_kind_it_arranges_lowercased() {
    // The path template has one placeholder and it is `{name}`, so the name is what decides
    // where the file is. A name that is not the kind lowercased puts the Endpoint form
    // somewhere the Portal does not look.
    let envelope: UiSchema =
        serde_norway::from_str(&DOCUMENTED.replace("  name: endpoint", "  name: forms"))
            .expect("it parses; the name is only wrong, not malformed");
    let Err(Error::Name { field, value, .. }) = envelope.validate() else {
        panic!("a UiSchema is named after the kind it arranges");
    };
    assert_eq!(field, "metadata.name");
    assert_eq!(value, "forms");

    // And a capitalised name is refused as the DNS-1123 label it is not, before that.
    let capitalised: UiSchema =
        serde_norway::from_str(&DOCUMENTED.replace("  name: endpoint", "  name: Endpoint"))
            .expect("it parses");
    assert!(
        matches!(capitalised.validate(), Err(Error::Name { field, .. }) if field == "metadata.name")
    );
}

#[test]
fn the_registry_resolves_the_kind_like_every_other() {
    let info = registry::by_kind("UiSchema").expect("UiSchema is in the catalogue");
    assert_eq!(info.plural, "uischemas");
    assert_eq!(
        info.repo_path("", "", "endpoint"),
        "portal/forms/endpoint.uischema.yaml"
    );
    // MF-09/CC-12: a published schema for every manifest family, which the catalogue gives
    // the kind for free once it is in the list.
    assert!(registry::schema_of("UiSchema").is_some());
}
