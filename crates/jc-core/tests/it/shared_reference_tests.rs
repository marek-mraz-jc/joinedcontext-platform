//! A SharedSpaceReference names its source by exactly one of `endpointRef` or `endpointSlug`
//! (EP-77, T-1448).

use jc_core::kinds::SharedSpaceReference;

fn reference(spec: &str) -> Result<SharedSpaceReference, String> {
    let yaml = format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: SharedSpaceReference\nmetadata:\n  name: city-bikes\n  namespace: helsinki-mobility\nspec:\n{spec}"
    );
    let parsed = SharedSpaceReference::from_yaml(&yaml).map_err(|e| e.to_string())?;
    parsed.validate().map_err(|e| e.to_string())?;
    Ok(parsed)
}

#[test]
fn a_ref_or_a_slug_alone_is_valid() {
    let by_ref = reference(
        "  endpointRef: { project: helsinki, name: helsinki-bikes }\n  alias: city-bikes\n",
    )
    .expect("valid");
    assert_eq!(
        by_ref
            .spec
            .endpoint_ref
            .as_ref()
            .map(|r| r.project.as_str()),
        Some("helsinki")
    );
    assert!(by_ref.spec.endpoint_slug.is_none());
    assert!(
        reference("  endpointSlug: scsd2eehkx42n53z2zyd6vshfh7s7irf\n  alias: city-bikes\n")
            .is_ok()
    );
}

#[test]
fn both_or_neither_is_refused() {
    let both = reference(
        "  endpointSlug: scsd2eehkx42n53z2zyd6vshfh7s7irf\n  endpointRef: { project: helsinki, name: helsinki-bikes }\n  alias: city-bikes\n",
    );
    assert!(both.unwrap_err().contains("exactly one"));
    assert!(reference("  alias: city-bikes\n")
        .unwrap_err()
        .contains("exactly one"));
}

#[test]
fn unicode_or_an_unknown_field_in_the_ref_is_refused() {
    assert!(
        reference("  endpointRef: { project: helsinki, name: pyörät }\n  alias: city-bikes\n")
            .is_err()
    );
    assert!(
        reference("  endpointRef: { project: helsinki, name: bikes }\n  alias: pyörät\n").is_err()
    );
    assert!(reference(
        "  endpointRef: { project: helsinki, name: bikes, slug: x }\n  alias: city-bikes\n"
    )
    .is_err());
}
