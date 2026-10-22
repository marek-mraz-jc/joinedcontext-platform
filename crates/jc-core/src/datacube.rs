//! Statistical datacubes as a connector kind (PL-112, T-5295).
//!
//! A `datacube` step names the JSON-stat 2.0 cubes a pipeline reads and the role of their
//! dimensions; the reconciler renders it into the one decoder every such pipeline ran as a
//! copied Bloblang mapping (`datacube.blobl`). One KeyPerformanceIndicator per published cell,
//! its id `{cube}-{area}-{period}-{indicator}[-{keys}][-{unit}]`, so a pipeline converted from
//! the copied mapping keeps its entities' URNs.

use crate::error::{Error, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::sync::LazyLock;

/// The decoder, with `{cubes}`, `{whole_area}` and `{dataset_page}` filled in by [`bloblang`].
const TEMPLATE: &str = include_str!("datacube.blobl");
/// The most cubes one step reads.
pub const MAX_CUBES: usize = 16;

/// DATAcube's table code, which the decoder finds in the answer's `href`.
static CUBE_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[a-z]{2}[0-9]{4}[a-z]{2}$").expect("valid regex"));
static DIMENSION_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[A-Za-z0-9_]{1,64}$").expect("valid regex"));
static AREA_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[A-Za-z0-9._~-]{1,32}$").expect("valid regex"));
static DATE_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$").expect("valid regex"));
/// An https address of printable ASCII without quotes, backslashes or `%`, which Bloblang's
/// `format` would read as a verb.
static URL_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"^https://[!#$&-\[\]-~]{1,2000}$"#).expect("valid regex"));

/// A `datacube` step (PL-112).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DatacubeStep {
    /// The territory code of a cube without an area dimension, `SK0` for Slovakia.
    pub whole_area: String,
    /// The catalogue page of a cube, `{cube}` standing for its code:
    /// `https://opendata.example.org/dataset/susr-{cube}`.
    pub dataset_page: String,
    /// The cubes by their code, `vh3001rr`.
    pub cubes: BTreeMap<String, Cube>,
}

/// One cube of a `datacube` step: which dimension plays which role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Cube {
    /// The address the cube is read from, as its DataSource reads it.
    pub source: String,
    /// The release date the publisher gives, `2025-10-31`; the answer's own `update` wins.
    #[serde(default)]
    pub update: Option<String>,
    /// The period dimension, `vh3001rr_rok`.
    #[serde(default)]
    pub period: Option<String>,
    /// The territory dimension; absent, every cell is `wholeArea`.
    #[serde(default)]
    pub area: Option<String>,
    /// The indicator dimension; absent, the cube's code is the indicator.
    #[serde(default)]
    pub indicator: Option<String>,
    /// The unit dimension, whose label is the cell's `unitText`.
    #[serde(default)]
    pub unit: Option<String>,
    /// Further dimensions, in order, that tell cells apart (`dimensionKey`).
    #[serde(default)]
    pub keys: Vec<String>,
}

/// The value is named in the reason, so the author sees what was refused.
fn invalid(field: impl Into<String>, value: &str, reason: &str) -> Error {
    Error::Invalid {
        field: field.into(),
        reason: format!("{value:?}: {reason}"),
    }
}

impl DatacubeStep {
    /// Checks every parameter before it reaches the rendered mapping.
    pub fn validate(&self) -> Result<()> {
        if !AREA_RE.is_match(&self.whole_area) {
            return Err(invalid(
                "spec.steps.datacube.wholeArea",
                &self.whole_area,
                "a territory code of letters, digits and . _ ~ -, such as SK0",
            ));
        }
        if !URL_RE.is_match(&self.dataset_page) || self.dataset_page.matches("{cube}").count() != 1
        {
            return Err(invalid(
                "spec.steps.datacube.datasetPage",
                &self.dataset_page,
                "an https address with {cube} once, such as https://opendata.example.org/dataset/susr-{cube}",
            ));
        }
        if self.cubes.is_empty() || self.cubes.len() > MAX_CUBES {
            return Err(invalid(
                "spec.steps.datacube.cubes",
                &self.cubes.len().to_string(),
                "one to 16 cubes",
            ));
        }
        self.cubes
            .iter()
            .try_for_each(|(code, cube)| cube.validate(code))
    }
}

impl Cube {
    fn validate(&self, code: &str) -> Result<()> {
        let field = |name: &str| format!("spec.steps.datacube.cubes.{code}.{name}");
        if !CUBE_RE.is_match(code) {
            return Err(invalid(
                "spec.steps.datacube.cubes",
                code,
                "a DATAcube table code such as vh3001rr",
            ));
        }
        if !URL_RE.is_match(&self.source) {
            return Err(invalid(field("source"), &self.source, "an https address"));
        }
        if let Some(update) = self.update.as_deref().filter(|u| !DATE_RE.is_match(u)) {
            return Err(invalid(
                field("update"),
                update,
                "a date such as 2025-10-31",
            ));
        }
        let roles = [&self.period, &self.area, &self.indicator, &self.unit];
        for name in roles.into_iter().flatten().chain(&self.keys) {
            if !DIMENSION_RE.is_match(name) {
                return Err(invalid(
                    field("dimensions"),
                    name,
                    "a dimension name of letters, digits and _, such as vh3001rr_rok",
                ));
            }
        }
        Ok(())
    }
}

/// The step's Bloblang mapping: the decoder with the step's cubes, area and catalogue page.
pub fn bloblang(step: &DatacubeStep) -> Result<String> {
    step.validate()?;
    let quoted = |s: &str| serde_json::to_string(s).unwrap_or_default();
    // The cube table last: what the step's own text holds is never read as a placeholder.
    Ok(TEMPLATE
        .replace("{whole_area}", &quoted(&step.whole_area))
        .replace(
            "{dataset_page}",
            &quoted(&step.dataset_page.replace("{cube}", "%v")),
        )
        .replace("{cubes}", &cube_table(step)))
}

/// The cubes as one JSON object with Python's `json.dumps(sort_keys=True)` spacing, as the copied
/// mappings held it, so a converted pipeline renders byte for byte what it ran.
fn cube_table(step: &DatacubeStep) -> String {
    let table: BTreeMap<&str, serde_json::Value> = step
        .cubes
        .iter()
        .map(|(code, cube)| {
            let row: BTreeMap<&str, serde_json::Value> = BTreeMap::from([
                ("area", cube.area.clone().into()),
                ("indicator", cube.indicator.clone().into()),
                ("keys", cube.keys.clone().into()),
                ("period", cube.period.clone().into()),
                ("source", cube.source.clone().into()),
                ("unit", cube.unit.clone().into()),
                ("update", cube.update.clone().into()),
            ]);
            (code.as_str(), serde_json::json!(row))
        })
        .collect();
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, Spaced);
    // Writing JSON values into a Vec fails only on a non-string map key, which none has.
    let _ = serde::Serialize::serialize(&table, &mut ser);
    String::from_utf8(out).unwrap_or_default()
}

/// `", "` between items and `": "` after a key, as Python writes JSON.
struct Spaced;

impl serde_json::ser::Formatter for Spaced {
    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            w.write_all(b", ")
        }
    }
    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            w.write_all(b", ")
        }
    }
    fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b": ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(area: &str, page: &str, cube: Cube) -> DatacubeStep {
        DatacubeStep {
            whole_area: area.to_owned(),
            dataset_page: page.to_owned(),
            cubes: BTreeMap::from([("vh3001rr".to_owned(), cube)]),
        }
    }

    fn cube(source: &str, update: Option<&str>, keys: &[&str]) -> Cube {
        Cube {
            source: source.to_owned(),
            update: update.map(str::to_owned),
            period: Some("vh3001rr_rok".to_owned()),
            area: None,
            indicator: None,
            unit: Some("vh3001rr_mj".to_owned()),
            keys: keys.iter().map(|k| (*k).to_owned()).collect(),
        }
    }

    const PAGE: &str = "https://o.example/d/{cube}";
    const SOURCE: &str = "https://data.statistics.sk/api/v2/dataset/vh3001rr/all";

    #[test]
    fn the_cube_table_is_written_as_python_writes_it() {
        let s = step("SK0", PAGE, cube(SOURCE, Some("2025-10-31"), &["a_b", "c"]));
        assert_eq!(
            cube_table(&s),
            r#"{"vh3001rr": {"area": null, "indicator": null, "keys": ["a_b", "c"], "period": "vh3001rr_rok", "source": "https://data.statistics.sk/api/v2/dataset/vh3001rr/all", "unit": "vh3001rr_mj", "update": "2025-10-31"}}"#
        );
    }

    #[test]
    fn each_parameter_is_checked_before_it_is_rendered() {
        assert!(bloblang(&step("SK0", PAGE, cube(SOURCE, None, &[]))).is_ok());
        let refusals = [
            step("S K", PAGE, cube(SOURCE, None, &[])),
            step("SK0", "https://o.example/d/", cube(SOURCE, None, &[])),
            step("SK0", PAGE, cube("ftp://x", None, &[])),
            step("SK0", PAGE, cube(SOURCE, Some("2025"), &[])),
            step("SK0", PAGE, cube(SOURCE, None, &["a-b"])),
        ];
        for s in refusals {
            assert!(s.validate().is_err(), "{s:?}");
        }
    }
}
