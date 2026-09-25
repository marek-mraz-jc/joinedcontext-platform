//! Compiles `data/unece-rec20.json` into a static table for `jc_core::units` (DM-06), so the
//! list is checked when the crate builds and nothing parses it on a request path.

use std::{env, fmt::Write, fs, path::Path};

fn text(value: &serde_json::Value, field: &str) -> String {
    value[field]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("unit `{field}` is not a string: {value}"))
}

fn optional(value: &serde_json::Value, field: &str) -> String {
    match &value[field] {
        serde_json::Value::Null => "None".to_owned(),
        serde_json::Value::String(s) => format!("Some({s:?})"),
        serde_json::Value::Number(n) => {
            format!("Some({})", float(n.as_f64().expect("a finite number")))
        }
        other => panic!("unit `{field}` is neither null, a string nor a number: {other}"),
    }
}

/// A float as its exact bits: the generated table never rounds, and a factor that happens to be
/// 2π or ln 2 is data, not a constant someone should have spelled `TAU`.
fn float(value: f64) -> String {
    format!("f64::from_bits({:#018x})", value.to_bits())
}

fn main() {
    let source = Path::new("data/unece-rec20.json");
    println!("cargo:rerun-if-changed={}", source.display());
    let document: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(source).expect("data/unece-rec20.json is readable"),
    )
    .expect("data/unece-rec20.json is JSON");
    let units = document["units"].as_array().expect("`units` is an array");

    let mut table = String::from("&[\n");
    let mut previous = String::new();
    for unit in units {
        let code = text(unit, "code");
        assert!(code > previous, "units are not sorted by code at `{code}`");
        let kinds: Vec<String> = unit["quantityKinds"]
            .as_array()
            .expect("`quantityKinds` is an array")
            .iter()
            .map(|k| format!("{:?}", k.as_str().expect("a quantity kind is a string")))
            .collect();
        writeln!(
            table,
            "    Unit {{ code: {code:?}, name: {:?}, symbol: {:?}, deprecated: {}, ucum: {}, qudt: {}, \
             quantity_kinds: &[{}], dimension: {}, factor: {}, offset: {}, frequent: {} }},",
            text(unit, "name"),
            text(unit, "symbol"),
            unit["deprecated"].as_bool().expect("`deprecated` is a boolean"),
            optional(unit, "ucum"),
            optional(unit, "qudt"),
            kinds.join(", "),
            optional(unit, "dimension"),
            optional(unit, "factor"),
            float(unit["offset"].as_f64().expect("`offset` is a number")),
            unit["frequent"].as_bool().expect("`frequent` is a boolean"),
        )
        .expect("writing to a String");
        previous = code;
    }
    table.push(']');

    let out = Path::new(&env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("units_table.rs");
    fs::write(out, table).expect("OUT_DIR is writable");
}
