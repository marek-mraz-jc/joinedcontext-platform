//! The small-talk and noise eval of T-4768 (`fixtures/brief/evals.json`): each input of the
//! owner's reference transcript lands in its class before the router, and a data question
//! reaches the router untouched.

use assistant::chat::brief::{brief, Brief};
use serde_json::Value;

const EVALS: &str = include_str!("fixtures/brief/evals.json");

#[test]
fn every_reference_input_lands_in_its_class() {
    let evals: Value = serde_json::from_str(EVALS).expect("the eval file is JSON");
    let cases = evals["cases"].as_array().expect("cases");
    assert!(
        cases.len() >= 9,
        "the eight reference inputs and a data question"
    );
    for case in cases {
        let input = case["input"].as_str().expect("input");
        let class = match brief(input) {
            None => "data",
            Some((Brief::Clarify, _)) => "clarify",
            Some(_) => "small-talk",
        };
        assert_eq!(class, case["class"].as_str().expect("class"), "{input:?}");
    }
}
