//! The `datacube` step (PL-112, T-5295): three committed ŠÚ SR pipelines, rewritten as parameters,
//! render byte for byte the decoder they ran as copied Bloblang, so their entities keep their URNs.

use jc_core::datacube::{bloblang, Cube, DatacubeStep};
use jc_core::kinds::{ComputeKind, Pipeline, Step};
use std::collections::BTreeMap;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/datacube");

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{FIXTURES}/{name}")).expect("fixture")
}

fn rendered(pipeline: &str) -> String {
    let pipeline = Pipeline::from_yaml(&fixture(pipeline)).expect("parses");
    pipeline.validate().expect("valid");
    let steps = pipeline.spec.steps();
    let [Step::Compute(compute)] = steps.as_slice() else {
        panic!("one compute step");
    };
    assert_eq!(compute.kind, ComputeKind::Datacube);
    assert!(compute.kind.is_mapping());
    compute
        .mapping()
        .expect("renders")
        .expect("a mapping")
        .into_owned()
}

#[test]
fn one_cube_without_keys_renders_the_committed_decoder() {
    assert_eq!(rendered("susr-vh.pipeline.yaml"), fixture("susr-vh.blobl"));
}

#[test]
fn a_cube_without_an_area_renders_the_committed_decoder() {
    assert_eq!(rendered("susr-as.pipeline.yaml"), fixture("susr-as.blobl"));
}

#[test]
fn two_cubes_read_in_slices_render_the_committed_decoder() {
    assert_eq!(
        rendered("susr-om-pohyb.pipeline.yaml"),
        fixture("susr-om-pohyb.blobl")
    );
}

fn step() -> DatacubeStep {
    DatacubeStep {
        whole_area: "SK0".to_owned(),
        dataset_page: "https://opendata.example.org/dataset/susr-{cube}".to_owned(),
        cubes: BTreeMap::from([(
            "vh3001rr".to_owned(),
            Cube {
                source: "https://data.statistics.sk/api/v2/dataset/vh3001rr/all/all/all".to_owned(),
                update: Some("2025-10-31".to_owned()),
                period: Some("vh3001rr_rok".to_owned()),
                area: None,
                indicator: None,
                unit: None,
                keys: vec![],
            },
        )]),
    }
}

fn refused(change: impl FnOnce(&mut DatacubeStep)) -> String {
    let mut s = step();
    change(&mut s);
    bloblang(&s).expect_err("refused").to_string()
}

#[test]
fn the_parameters_reach_the_mapping_only_as_quoted_literals() {
    let mapping = bloblang(&step()).expect("renders");
    assert!(mapping.contains(r#""https://opendata.example.org/dataset/susr-%v".format($cube)"#));
    assert!(mapping.contains(r#"if $roles.area == null { "SK0" }"#));
    assert!(!mapping.contains("{cubes}") && !mapping.contains("{whole_area}"));
}

#[test]
fn a_parameter_that_could_break_out_of_its_literal_is_refused() {
    let quote = refused(|s| s.whole_area = "SK0\" } else { env(\"X\")".to_owned());
    assert!(quote.contains("spec.steps.datacube.wholeArea"), "{quote}");
    let verb = refused(|s| s.dataset_page = "https://x.org/%v/{cube}".to_owned());
    assert!(verb.contains("spec.steps.datacube.datasetPage"), "{verb}");
    let plain = refused(|s| s.dataset_page = "http://x.org/{cube}".to_owned());
    assert!(plain.contains("datasetPage"), "{plain}");
    let twice = refused(|s| s.dataset_page = "https://x.org/{cube}/{cube}".to_owned());
    assert!(twice.contains("datasetPage"), "{twice}");
    let dimension = refused(|s| {
        if let Some(cube) = s.cubes.get_mut("vh3001rr") {
            cube.keys.push("a\"b".to_owned());
        }
    });
    assert!(
        dimension.contains("cubes.vh3001rr.dimensions"),
        "{dimension}"
    );
    let source = refused(|s| {
        if let Some(cube) = s.cubes.get_mut("vh3001rr") {
            cube.source = "https://x.org/\"".to_owned();
        }
    });
    assert!(source.contains("cubes.vh3001rr.source"), "{source}");
    let update = refused(|s| {
        if let Some(cube) = s.cubes.get_mut("vh3001rr") {
            cube.update = Some("31.10.2025".to_owned());
        }
    });
    assert!(update.contains("cubes.vh3001rr.update"), "{update}");
}

#[test]
fn a_cube_code_the_decoder_cannot_find_and_an_empty_step_are_refused() {
    let code = refused(|s| {
        let cube = s.cubes.remove("vh3001rr").expect("cube");
        s.cubes.insert("VH-3001".to_owned(), cube);
    });
    assert!(code.contains("VH-3001"), "{code}");
    let none = refused(|s| s.cubes.clear());
    assert!(none.contains("one to 16 cubes"), "{none}");
}

#[test]
fn a_datacube_step_without_its_parameters_or_parameters_on_another_kind_are_refused() {
    let without = fixture("susr-vh.pipeline.yaml").replace("kind: datacube", "kind: bloblang");
    let error = match Pipeline::from_yaml(&without) {
        Err(error) => error.to_string(),
        Ok(pipeline) => pipeline.validate().expect_err("refused").to_string(),
    };
    assert!(error.contains("spec.steps.datacube"), "{error}");
    let unknown = fixture("susr-vh.pipeline.yaml")
        .replace("wholeArea: SK0", "wholeArea: SK0\n        town: x");
    assert!(Pipeline::from_yaml(&unknown).is_err());
}

#[test]
fn a_placeholder_in_a_parameter_and_a_runner_read_in_a_source_are_refused() {
    // integrator-2 on T-5295: `{cubes}` in datasetPage put the cube table outside a string literal.
    let placeholder = refused(|s| s.dataset_page = "https://o.example/d/{cubes}{cube}".to_owned());
    assert!(placeholder.contains("datasetPage"), "{placeholder}");
    let read = refused(|s| {
        if let Some(cube) = s.cubes.get_mut("vh3001rr") {
            cube.source = "https://x.org/env(this.a)".to_owned();
        }
    });
    assert!(read.contains("spec.steps.datacube"), "{read}");
}
