//! Indicators over several sources (PL-102…PL-105, T-4864).
//!
//! A `kpi` step names two to five sources by their `name`, a join (`time`, `entity`, `area`) and
//! a formula over aggregates of their attributes, `sum(parking.occupiedSpotNumber) /
//! count(stops)`. This module parses and checks the formula, computes it over pages of entities
//! (the Portal's preview), and compiles it to the Bloblang mapping the runner folds the run's
//! pages with. Both paths refuse the same inputs with the same words: a division by zero, an
//! average over nothing or a source that did not answer writes no value (PL-104).

use crate::error::{Error, Result};
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::LazyLock;

/// The fewest and most sources one indicator reads (PL-102).
pub const MIN_SOURCES: usize = 2;
/// The most sources one indicator reads (PL-102).
pub const MAX_SOURCES: usize = 5;
/// The longest formula (PL-103).
pub const MAX_FORMULA_CHARS: usize = 512;
/// The most aggregate terms in one formula (PL-103).
pub const MAX_TERMS: usize = 64;
/// The most indicators one run of an entity or area join writes (PL-104).
pub const MAX_KEYS: usize = 500;
/// The longest indicator name, leaving room for a key's slug in the `{localId}`.
pub const MAX_NAME_CHARS: usize = 48;
/// The longest key slug appended to the name.
const MAX_SLUG_CHARS: usize = 40;
/// The window an indicator covers when the step names none.
pub const DEFAULT_WINDOW: &str = "PT1H";
/// The UN/CEFACT codes a dimensionless result takes: "one" and percent.
pub const DIMENSIONLESS: [&str; 2] = ["C62", "P1"];

static WINDOW_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^P(T[1-9][0-9]{0,3}[HM]|[1-9][0-9]{0,2}D)$").expect("valid regex")
});
static UNIT_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[A-Z0-9]{1,8}$").expect("valid regex"));

/// A `kpi` step (PL-102).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KpiStep {
    /// The indicator's `{localId}`; a key's slug is appended for an entity or area join.
    pub name: String,
    /// A title for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Arithmetic over `sum|avg|min|max(input.attribute)` and `count(input)` (PL-103).
    pub formula: String,
    /// The result's UN/CEFACT common code.
    pub unit_code: String,
    /// How the sources' rows meet; `by: time` when absent.
    #[serde(default)]
    pub join: Join,
    /// The window the value covers, `PT15M`, `PT1H`, `P1D`; `PT1H` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
}

/// How the sources' rows meet (PL-102, PL-104).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Join {
    /// `time`, `entity` or `area`.
    #[serde(default)]
    pub by: JoinBy,
    /// Per source name, the attribute (or `id`) naming the entity or area a row is about.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<String, String>,
}

/// The join kinds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum JoinBy {
    /// Every row of the run's window, one indicator.
    #[default]
    Time,
    /// One indicator per entity every source names.
    Entity,
    /// One indicator per area every source has rows in.
    Area,
}

/// What admission and the compiler know of one source: its name, attributes and their units.
#[derive(Debug, Clone, Default)]
pub struct Input {
    /// The source's `name`.
    pub name: String,
    /// The entity type it reads.
    pub entity_type: String,
    /// `query.attrs`: the attributes a term may name.
    pub attrs: Vec<String>,
    /// The `unit_code` of an attribute, from the space's model, when it has one.
    pub units: BTreeMap<String, String>,
    /// The source Endpoint's name; its space segment is the renderer's `JC_SOURCE_SPACE`
    /// (the first source), `JC_SOURCE_SPACE_2`, … (PL-57).
    pub endpoint: String,
    /// The source's `q`, for the provenance.
    pub q: Option<String>,
}

/// The aggregate of a term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    /// The sum of the numbers.
    Sum,
    /// Their mean; an average over nothing is refused.
    Avg,
    /// The smallest.
    Min,
    /// The largest.
    Max,
    /// The rows, or the rows with a number in the attribute.
    Count,
}

impl Agg {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "sum" => Self::Sum,
            "avg" => Self::Avg,
            "min" => Self::Min,
            "max" => Self::Max,
            "count" => Self::Count,
            _ => return None,
        })
    }

    const fn word(self) -> &'static str {
        match self {
            Self::Sum => "sum",
            Self::Avg => "avg",
            Self::Min => "min",
            Self::Max => "max",
            Self::Count => "count",
        }
    }
}

/// A parsed formula.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A decimal number.
    Num(f64),
    /// `agg(input.attr)` or `count(input)`.
    Term {
        /// The aggregate.
        agg: Agg,
        /// The source name.
        input: String,
        /// The attribute; only `count` goes without.
        attr: Option<String>,
    },
    /// `-x`.
    Neg(Box<Expr>),
    /// `a op b`.
    Bin(char, Box<Expr>, Box<Expr>),
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Num(n) => write!(f, "{n}"),
            Self::Term { agg, input, attr } => match attr {
                Some(attr) => write!(f, "{}({input}.{attr})", agg.word()),
                None => write!(f, "{}({input})", agg.word()),
            },
            Self::Neg(inner) => write!(f, "-{inner}"),
            Self::Bin(op, a, b) => write!(f, "({a} {op} {b})"),
        }
    }
}

fn invalid(reason: impl Into<String>) -> Error {
    Error::Invalid {
        field: "spec.steps.kpi.formula".to_owned(),
        reason: reason.into(),
    }
}

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
    terms: usize,
}

impl Parser<'_> {
    fn skip(&mut self) {
        while self.text.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip();
        self.text.get(self.at).copied()
    }

    fn word(&mut self) -> String {
        self.skip();
        let start = self.at;
        while self
            .text
            .get(self.at)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
        {
            self.at += 1;
        }
        String::from_utf8_lossy(&self.text[start..self.at]).into_owned()
    }

    fn expect(&mut self, c: u8) -> Result<()> {
        if self.peek() == Some(c) {
            self.at += 1;
            return Ok(());
        }
        Err(invalid(format!(
            "expected `{}` at character {}",
            c as char,
            self.at + 1
        )))
    }

    fn sum(&mut self) -> Result<Expr> {
        let mut left = self.product()?;
        while let Some(op @ (b'+' | b'-')) = self.peek() {
            self.at += 1;
            left = Expr::Bin(op as char, Box::new(left), Box::new(self.product()?));
        }
        Ok(left)
    }

    fn product(&mut self) -> Result<Expr> {
        let mut left = self.factor()?;
        while let Some(op @ (b'*' | b'/')) = self.peek() {
            self.at += 1;
            left = Expr::Bin(op as char, Box::new(left), Box::new(self.factor()?));
        }
        Ok(left)
    }

    fn factor(&mut self) -> Result<Expr> {
        match self.peek() {
            Some(b'(') => {
                self.at += 1;
                let inner = self.sum()?;
                self.expect(b')')?;
                Ok(inner)
            }
            Some(b'-') => {
                self.at += 1;
                Ok(Expr::Neg(Box::new(self.factor()?)))
            }
            Some(c) if c.is_ascii_digit() => {
                let start = self.at;
                while self
                    .text
                    .get(self.at)
                    .is_some_and(|c| c.is_ascii_digit() || *c == b'.')
                {
                    self.at += 1;
                }
                let text = String::from_utf8_lossy(&self.text[start..self.at]);
                text.parse::<f64>()
                    .map(Expr::Num)
                    .map_err(|_| invalid(format!("`{text}` is not a number")))
            }
            Some(c) if c.is_ascii_alphabetic() => {
                let word = self.word();
                let agg = Agg::parse(&word).ok_or_else(|| {
                    invalid(format!(
                        "`{word}` is not a function: use sum, avg, min, max or count"
                    ))
                })?;
                self.expect(b'(')?;
                let input = self.word();
                let attr = if self.peek() == Some(b'.') {
                    self.at += 1;
                    Some(self.word())
                } else {
                    None
                };
                self.expect(b')')?;
                if input.is_empty() || attr.as_deref() == Some("") {
                    return Err(invalid(format!(
                        "{word}() names a source and an attribute: {word}(source.attribute)"
                    )));
                }
                if attr.is_none() && agg != Agg::Count {
                    return Err(invalid(format!(
                        "{word}({input}) needs an attribute: {word}({input}.attribute)"
                    )));
                }
                self.terms += 1;
                Ok(Expr::Term { agg, input, attr })
            }
            Some(c) => Err(invalid(format!(
                "`{}` at character {} is not part of a formula",
                c as char,
                self.at + 1
            ))),
            None => Err(invalid("the formula ends too early")),
        }
    }
}

/// Parses a formula (PL-103).
pub fn parse(formula: &str) -> Result<Expr> {
    if formula.trim().is_empty() {
        return Err(invalid(
            "write a formula, such as sum(parking.occupied) / count(stops)",
        ));
    }
    if formula.chars().count() > MAX_FORMULA_CHARS {
        return Err(invalid(format!(
            "a formula has at most {MAX_FORMULA_CHARS} characters"
        )));
    }
    let mut parser = Parser {
        text: formula.as_bytes(),
        at: 0,
        terms: 0,
    };
    let expr = parser.sum()?;
    if parser.peek().is_some() {
        return Err(invalid(format!(
            "unexpected `{}` at character {}",
            formula[parser.at..].chars().next().unwrap_or(' '),
            parser.at + 1
        )));
    }
    if parser.terms == 0 {
        return Err(invalid("a formula reads at least one source"));
    }
    if parser.terms > MAX_TERMS {
        return Err(invalid(format!("a formula has at most {MAX_TERMS} terms")));
    }
    Ok(expr)
}

/// A unit as exponents of UN/CEFACT codes; `None` when a slot has no unit in the model.
type Unit = Option<BTreeMap<String, i32>>;

fn unit_of(expr: &Expr, inputs: &[Input]) -> Result<Unit> {
    Ok(match expr {
        Expr::Num(_)
        | Expr::Term {
            agg: Agg::Count, ..
        } => Some(BTreeMap::new()),
        Expr::Term { input, attr, .. } => {
            let code = attr.as_ref().and_then(|attr| {
                inputs
                    .iter()
                    .find(|i| &i.name == input)
                    .and_then(|i| i.units.get(attr))
            });
            code.map(|code| {
                let mut unit = BTreeMap::new();
                if !DIMENSIONLESS.contains(&code.as_str()) {
                    unit.insert(code.clone(), 1);
                }
                unit
            })
        }
        Expr::Neg(inner) => unit_of(inner, inputs)?,
        Expr::Bin(op, a, b) => {
            let (ua, ub) = (unit_of(a, inputs)?, unit_of(b, inputs)?);
            match op {
                '+' | '-' => match (ua, ub) {
                    (Some(x), Some(y)) if x != y && !(x.is_empty() || y.is_empty()) => {
                        return Err(invalid(format!(
                            "{a} is in {} and {b} in {}: add or subtract only values of one unit",
                            show(&x),
                            show(&y)
                        )))
                    }
                    (Some(x), Some(y)) => Some(if x.is_empty() { y } else { x }),
                    (x, y) => x.or(y),
                },
                _ => match (ua, ub) {
                    (Some(mut x), Some(y)) => {
                        let sign = if *op == '*' { 1 } else { -1 };
                        for (code, power) in y {
                            *x.entry(code).or_insert(0) += sign * power;
                        }
                        x.retain(|_, power| *power != 0);
                        Some(x)
                    }
                    _ => None,
                },
            }
        }
    })
}

fn show(unit: &BTreeMap<String, i32>) -> String {
    if unit.is_empty() {
        return "no unit".to_owned();
    }
    unit.iter()
        .map(|(code, power)| match power {
            1 => code.clone(),
            p => format!("{code}^{p}"),
        })
        .collect::<Vec<_>>()
        .join("·")
}

fn terms<'e>(expr: &'e Expr, out: &mut Vec<(&'e str, Option<&'e str>, Agg)>) {
    match expr {
        Expr::Num(_) => {}
        Expr::Term { agg, input, attr } => out.push((input, attr.as_deref(), *agg)),
        Expr::Neg(inner) => terms(inner, out),
        Expr::Bin(_, a, b) => {
            terms(a, out);
            terms(b, out);
        }
    }
}

fn field(name: &str, reason: impl Into<String>) -> Error {
    Error::Invalid {
        field: format!("spec.steps.kpi.{name}"),
        reason: reason.into(),
    }
}

impl KpiStep {
    /// The window, the default when absent.
    pub fn window(&self) -> &str {
        self.window.as_deref().unwrap_or(DEFAULT_WINDOW)
    }

    /// Admission (PL-102, PL-103): every name, the join, the formula and its unit.
    pub fn validate(&self, inputs: &[Input]) -> Result<Expr> {
        names::validate_dns1123_label(&self.name)?;
        if self.name.len() > MAX_NAME_CHARS {
            return Err(field(
                "name",
                format!("an indicator name has at most {MAX_NAME_CHARS} characters"),
            ));
        }
        if !(MIN_SOURCES..=MAX_SOURCES).contains(&inputs.len()) {
            return Err(Error::Invalid {
                field: "spec.sources".to_owned(),
                reason: format!(
                    "an indicator over several sources reads {MIN_SOURCES} to {MAX_SOURCES} of them; \
                     one source is the kpi preset of PL-45"
                ),
            });
        }
        let mut seen = BTreeSet::new();
        for input in inputs {
            if input.name.is_empty() {
                return Err(Error::Invalid {
                    field: "spec.sources.name".to_owned(),
                    reason: "each source of a kpi step has a name the formula reads it by"
                        .to_owned(),
                });
            }
            if !seen.insert(input.name.as_str()) {
                return Err(Error::Invalid {
                    field: "spec.sources.name".to_owned(),
                    reason: format!("two sources are named `{}`", input.name),
                });
            }
        }
        if let Some(window) = &self.window {
            if !WINDOW_RE.is_match(window) {
                return Err(field(
                    "window",
                    format!("`{window}` is not a window such as PT15M, PT1H or P1D"),
                ));
            }
        }
        if !UNIT_RE.is_match(&self.unit_code) {
            return Err(field(
                "unitCode",
                format!(
                    "`{}` is not a UN/CEFACT common code such as C62 (a count) or P1 (percent)",
                    self.unit_code
                ),
            ));
        }
        match self.join.by {
            JoinBy::Time if !self.join.keys.is_empty() => {
                return Err(field("join.keys", "a join by time takes no keys"));
            }
            JoinBy::Time => {}
            JoinBy::Entity | JoinBy::Area => {
                for input in inputs {
                    let Some(key) = self.join.keys.get(&input.name) else {
                        return Err(field(
                            "join.keys",
                            format!("name the attribute of `{}` the rows meet on", input.name),
                        ));
                    };
                    if key != "id" && !input.attrs.contains(key) {
                        return Err(field(
                            "join.keys",
                            format!(
                                "`{key}` is not among the attributes `{}` reads ({})",
                                input.name,
                                input.attrs.join(", ")
                            ),
                        ));
                    }
                }
                if let Some(extra) = self
                    .join
                    .keys
                    .keys()
                    .find(|name| !inputs.iter().any(|i| &i.name == *name))
                {
                    return Err(field(
                        "join.keys",
                        format!("`{extra}` is not one of the sources"),
                    ));
                }
            }
        }
        let expr = parse(&self.formula)?;
        let mut used = Vec::new();
        terms(&expr, &mut used);
        for (name, attr, agg) in &used {
            let Some(input) = inputs.iter().find(|i| i.name == *name) else {
                return Err(invalid(format!(
                    "`{name}` is not a source: the sources are {}",
                    inputs
                        .iter()
                        .map(|i| i.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            };
            if let Some(attr) = attr {
                if !input.attrs.iter().any(|a| a == attr) {
                    return Err(invalid(format!(
                        "{}({name}.{attr}): `{attr}` is not among the attributes `{name}` reads ({})",
                        agg.word(),
                        input.attrs.join(", ")
                    )));
                }
            }
        }
        match unit_of(&expr, inputs)? {
            Some(unit) if unit.is_empty() && !DIMENSIONLESS.contains(&self.unit_code.as_str()) => {
                Err(field(
                    "unitCode",
                    format!(
                        "the formula has no unit, so the indicator is C62 or P1, not {}",
                        self.unit_code
                    ),
                ))
            }
            Some(unit) if unit.len() == 1 && unit.values().all(|p| *p == 1) => {
                let code = unit.keys().next().cloned().unwrap_or_default();
                if code == self.unit_code {
                    Ok(expr)
                } else {
                    Err(field(
                        "unitCode",
                        format!(
                            "the formula is in {code}, so the indicator is {code}, not {}",
                            self.unit_code
                        ),
                    ))
                }
            }
            _ => Ok(expr),
        }
    }
}

/// The number an attribute holds on a row: a Property's `value` or a keyValues number.
fn number(row: &Value, attr: &str) -> Option<f64> {
    let member = row.get(attr)?;
    member
        .get("value")
        .unwrap_or(member)
        .as_f64()
        .filter(|n| n.is_finite())
}

/// The key a row meets the other sources on: `id`, a Relationship's object, a Property's value.
fn key(row: &Value, attr: &str) -> Option<String> {
    let member = row.get(attr)?;
    let member = member
        .get("value")
        .or_else(|| member.get("object"))
        .unwrap_or(member);
    match member {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn eval(expr: &Expr, rows: &BTreeMap<&str, Vec<&Value>>) -> std::result::Result<f64, String> {
    Ok(match expr {
        Expr::Num(n) => *n,
        Expr::Term { agg, input, attr } => {
            let rows = rows.get(input.as_str()).map(Vec::as_slice).unwrap_or(&[]);
            let Some(attr) = attr else {
                return Ok(rows.len() as f64);
            };
            let values: Vec<f64> = rows.iter().filter_map(|row| number(row, attr)).collect();
            match agg {
                Agg::Count => values.len() as f64,
                Agg::Sum => values.iter().sum(),
                _ if values.is_empty() => return Err(format!("{expr} has no values")),
                Agg::Avg => values.iter().sum::<f64>() / values.len() as f64,
                Agg::Min => values.iter().copied().fold(f64::INFINITY, f64::min),
                Agg::Max => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            }
        }
        Expr::Neg(inner) => -eval(inner, rows)?,
        Expr::Bin(op, a, b) => {
            let (x, y) = (eval(a, rows)?, eval(b, rows)?);
            match op {
                '+' => x + y,
                '-' => x - y,
                '*' => x * y,
                _ if y == 0.0 => return Err(format!("division by zero: {b} is 0")),
                _ => x / y,
            }
        }
    })
}

/// The slug of a key in an indicator's `{localId}`: what the compiled mapping writes too.
pub fn slug(key: &str) -> String {
    let mut out = String::new();
    for c in key.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(MAX_SLUG_CHARS).collect();
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        "x".to_owned()
    } else {
        out.to_owned()
    }
}

/// One computed value: the key it is for (`None` for a join by time) and the number.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Computed {
    /// The entity or area key.
    pub key: Option<String>,
    /// The indicator's `{localId}`.
    pub name: String,
    /// The value.
    pub value: f64,
}

/// Computes the step over each source's page (the Portal's preview, PL-105). A missing source,
/// a division by zero or an average over nothing is the run's problem, and nothing is computed.
pub fn evaluate(
    step: &KpiStep,
    expr: &Expr,
    pages: &BTreeMap<String, Vec<Value>>,
) -> std::result::Result<Vec<Computed>, String> {
    for name in step.join.keys.keys() {
        if !pages.contains_key(name) {
            return Err(format!("input {name} did not answer in this run"));
        }
    }
    let mut used = Vec::new();
    terms(expr, &mut used);
    for (name, _, _) in &used {
        if !pages.contains_key(*name) {
            return Err(format!("input {name} did not answer in this run"));
        }
    }
    if step.join.by == JoinBy::Time {
        let rows = pages
            .iter()
            .map(|(name, rows)| (name.as_str(), rows.iter().collect()))
            .collect();
        return Ok(vec![Computed {
            key: None,
            name: step.name.clone(),
            value: eval(expr, &rows)?,
        }]);
    }
    let mut common: Option<BTreeSet<String>> = None;
    for (name, attr) in &step.join.keys {
        let keys: BTreeSet<String> = pages[name].iter().filter_map(|r| key(r, attr)).collect();
        common = Some(match common {
            None => keys,
            Some(before) => before.intersection(&keys).cloned().collect(),
        });
    }
    let common = common.unwrap_or_default();
    if common.len() > MAX_KEYS {
        return Err(format!(
            "{} keys meet in every source; one run writes at most {MAX_KEYS}",
            common.len()
        ));
    }
    common
        .into_iter()
        .map(|k| {
            let rows = step
                .join
                .keys
                .iter()
                .map(|(name, attr)| {
                    let rows = pages[name]
                        .iter()
                        .filter(|row| key(row, attr).as_deref() == Some(k.as_str()))
                        .collect();
                    (name.as_str(), rows)
                })
                .collect();
            Ok(Computed {
                name: format!("{}-{}", step.name, slug(&k)),
                value: eval(expr, &rows)?,
                key: Some(k),
            })
        })
        .collect()
}

/// A Bloblang string literal.
fn lit(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

/// The Bloblang query reading `attr` of row `e` as a key.
fn key_query(attr: &str) -> String {
    if attr == "id" {
        "e.id".to_owned()
    } else {
        format!("e.{attr}.value.or(e.{attr}.object).or(e.{attr})")
    }
}

fn values(rows: &str, attr: &str) -> String {
    format!("{rows}.map_each(e -> e.{attr}.value.or(e.{attr})).filter(v -> v.type() == \"number\")")
}

/// `expr` as a Bloblang query over the per-source row variables `rows(name)`.
fn query(expr: &Expr, rows: &dyn Fn(&str) -> String) -> String {
    match expr {
        Expr::Num(n) => format!("{n:?}"),
        Expr::Term { agg, input, attr } => {
            let rows = rows(input);
            let Some(attr) = attr else {
                return format!("{rows}.length()");
            };
            let values = values(&rows, attr);
            let empty = lit(&format!("{expr} has no values"));
            match agg {
                Agg::Count => format!("{values}.length()"),
                Agg::Sum => format!("{values}.sum()"),
                Agg::Avg => format!(
                    "(if {values}.length() == 0 {{ throw({empty}) }} else {{ {values}.sum() / {values}.length() }})"
                ),
                Agg::Min | Agg::Max => format!(
                    "(if {values}.length() == 0 {{ throw({empty}) }} else {{ {values}.{}() }})",
                    agg.word()
                ),
            }
        }
        Expr::Neg(inner) => format!("(0 - {})", query(inner, rows)),
        Expr::Bin('/', a, b) => {
            let below = query(b, rows);
            format!(
                "({} / (if {below} == 0 {{ throw({}) }} else {{ {below} }}))",
                query(a, rows),
                lit(&format!("division by zero: {b} is 0"))
            )
        }
        Expr::Bin(op, a, b) => format!("({} {op} {})", query(a, rows), query(b, rows)),
    }
}

/// The mapping each source's page is wrapped with before the broker batches the run's pages:
/// `{input, entities}`, or `{input, error}` when the read failed (PL-104).
pub fn wrap_mapping(input: &str) -> String {
    format!(
        "root = if this.type() == \"object\" && this.exists(\"jc_kpi_error\") {{ \
         {{ \"input\": {name}, \"error\": this.jc_kpi_error }} }} else {{ \
         {{ \"input\": {name}, \"entities\": if this.type() == \"array\" {{ this }} else {{ [this] }} }} }}",
        name = lit(input)
    )
}

/// Who computes the indicator, for `computedBy`. The indicator space is the first output's,
/// `env("JC_SPACE")`, which the renderer writes in as the segment (PL-57).
#[derive(Debug, Clone)]
pub struct Target<'a> {
    /// The pipeline's project.
    pub pipeline_space: &'a str,
    /// The pipeline's name.
    pub pipeline: &'a str,
}

/// The variable source `index` (0-based) reads its space segment from (PL-57).
fn source_space(index: usize) -> String {
    match index {
        0 => "env(\"JC_SOURCE_SPACE\")".to_owned(),
        n => format!("env(\"JC_SOURCE_SPACE_{}\")", n + 1),
    }
}

/// Compiles a checked step to the mapping that folds the run's batch of wrapped pages into one
/// indicator per run or per key (PL-104). The org domain is read from `JC_ORG_DOMAIN`.
pub fn bloblang(step: &KpiStep, expr: &Expr, inputs: &[Input], target: &Target<'_>) -> String {
    let var = |name: &str| format!("$in_{}", name.replace('-', "_"));
    let mut out = String::from(
        "let domain = env(\"JC_ORG_DOMAIN\")\nlet now = now()\nlet pages = this.or([])\n",
    );
    let names: Vec<String> = inputs.iter().map(|i| lit(&i.name)).collect();
    out.push_str(&format!(
        "let missing = [{}].filter(n -> $pages.filter(p -> p.input == n).length() == 0)\n\
         let failed = $pages.filter(p -> p.error != null).map_each(p -> \"input \" + p.input + \": \" + p.error.string())\n\
         let problem = if $missing.length() > 0 {{ throw(\"input \" + $missing.join(\", \") + \" did not answer in this run\") }} \
         else if $failed.length() > 0 {{ throw($failed.join(\"; \")) }} else {{ false }}\n",
        names.join(", ")
    ));
    for input in inputs {
        out.push_str(&format!(
            "let in_{} = $pages.filter(p -> p.input == {}).index(0).entities.or([])\n",
            input.name.replace('-', "_"),
            lit(&input.name)
        ));
    }
    let provenance = |rows: &dyn Fn(&str) -> String| -> String {
        let mut text = lit(step.formula.trim());
        for (index, input) in inputs.iter().enumerate() {
            let q = input
                .q
                .as_deref()
                .map(|q| format!(" where {q}"))
                .unwrap_or_default();
            text.push_str(&format!(
                " + {} + $domain + \":\" + {} + {} + {}.length().string() + \" rows\"",
                lit(&format!(
                    "; {}: {}{q} from urn:ngsi-ld:Endpoint:",
                    input.name, input.entity_type
                )),
                source_space(index),
                lit(&format!(":{}, ", input.endpoint)),
                rows(&input.name)
            ));
        }
        text
    };
    let derived: Vec<String> = inputs
        .iter()
        .enumerate()
        .map(|(index, i)| {
            format!(
                "\"urn:ngsi-ld:Endpoint:\" + $domain + \":\" + {} + {}",
                source_space(index),
                lit(&format!(":{}", i.endpoint))
            )
        })
        .collect();
    let entity = |name: &str, rows: &dyn Fn(&str) -> String| -> String {
        format!(
            "{{\n  \"id\": \"urn:ngsi-ld:KeyPerformanceIndicator:\" + $domain + \":\" + env(\"JC_SPACE\") + \":\" + {name},\n  \
             \"type\": \"KeyPerformanceIndicator\",\n  \
             \"name\": {{ \"type\": \"Property\", \"value\": {name} }},\n  \
             \"calculationFormula\": {{ \"type\": \"Property\", \"value\": {formula} }},\n  \
             \"currentValue\": {{ \"type\": \"Property\", \"value\": {value}, \"unitCode\": {unit}, \"observedAt\": $now }},\n  \
             \"calculationPeriod\": {{ \"type\": \"Property\", \"value\": {{ \"start\": $now.ts_sub_iso8601({window}), \"end\": $now }} }},\n  \
             \"updatedAt\": {{ \"type\": \"Property\", \"value\": {{ \"@type\": \"DateTime\", \"@value\": $now }} }},\n  \
             \"derivedFrom\": {{ \"type\": \"Property\", \"value\": [{derived}] }},\n  \
             \"computedBy\": {{ \"type\": \"Property\", \"value\": \"urn:ngsi-ld:Pipeline:\" + $domain + {pipeline} }}\n}}",
            formula = provenance(rows),
            value = query(expr, rows),
            unit = lit(&step.unit_code),
            window = lit(step.window()),
            derived = derived.join(", "),
            pipeline = lit(&format!(":{}:{}", target.pipeline_space, target.pipeline)),
        )
    };
    if step.join.by == JoinBy::Time {
        out.push_str(&format!("root = [{}]\n", entity(&lit(&step.name), &var)));
        return out;
    }
    for input in inputs {
        let attr = step.join.keys.get(&input.name).map_or("id", String::as_str);
        out.push_str(&format!(
            "let keys_{n} = {rows}.map_each(e -> {key}).filter(k -> k != null && k.string() != \"\").map_each(k -> k.string()).unique()\n",
            n = input.name.replace('-', "_"),
            rows = var(&input.name),
            key = key_query(attr),
        ));
    }
    let first = inputs[0].name.replace('-', "_");
    let mut keys = format!("$keys_{first}");
    for input in &inputs[1..] {
        keys.push_str(&format!(
            ".filter(k -> $keys_{}.contains(k))",
            input.name.replace('-', "_")
        ));
    }
    out.push_str(&format!(
        "let keys = {keys}.sort()\n\
         let too_many = if $keys.length() > {MAX_KEYS} {{ throw($keys.length().string() + \" keys meet in every source; one run writes at most {MAX_KEYS}\") }} else {{ false }}\n"
    ));
    let grouped = |name: &str| {
        let attr = step.join.keys.get(name).map_or("id", String::as_str);
        format!(
            "{}.filter(e -> ({}).string() == k)",
            var(name),
            key_query(attr)
        )
    };
    let slug = format!(
        "{} + \"-\" + k.lowercase().re_replace_all(\"[^a-z0-9]+\", \"-\").trim(\"-\").apply(\"jc_slug\")",
        lit(&step.name)
    );
    out.insert_str(
        0,
        &format!(
            "map jc_slug {{\n  let s = if this.length() > {MAX_SLUG_CHARS} {{ this.slice(0, {MAX_SLUG_CHARS}) }} else {{ this }}\n  \
             let s = $s.trim(\"-\")\n  root = if $s == \"\" {{ \"x\" }} else {{ $s }}\n}}\n"
        ),
    );
    out.push_str(&format!(
        "root = $keys.map_each(k -> {})\n",
        entity(&format!("({slug})"), &grouped)
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inputs() -> Vec<Input> {
        vec![
            Input {
                name: "parking".into(),
                entity_type: "ParkingSite".into(),
                attrs: vec!["occupied".into(), "capacity".into(), "district".into()],
                units: BTreeMap::new(),
                endpoint: "helsinki-parking".into(),
                q: None,
            },
            Input {
                name: "stops".into(),
                entity_type: "TransitStop".into(),
                attrs: vec!["district".into(), "wait".into()],
                units: BTreeMap::from([("wait".into(), "MIN".into())]),
                endpoint: "helsinki-transit".into(),
                q: Some("district!=null".into()),
            },
        ]
    }

    fn step(formula: &str, unit: &str) -> KpiStep {
        KpiStep {
            name: "parking-per-stop".into(),
            title: None,
            formula: formula.into(),
            unit_code: unit.into(),
            join: Join::default(),
            window: None,
        }
    }

    #[test]
    fn a_formula_parses_with_precedence_and_prints_back() {
        let expr = parse("sum(parking.occupied) / count(stops) * 100 - -1").expect("parses");
        assert_eq!(
            expr.to_string(),
            "(((sum(parking.occupied) / count(stops)) * 100) - -1)"
        );
    }

    #[test]
    fn a_name_the_sources_do_not_declare_is_refused_with_the_list() {
        let err = step("sum(cars.occupied)", "C62")
            .validate(&inputs())
            .expect_err("unknown input");
        assert!(err.to_string().contains("`cars` is not a source"), "{err}");
        let err = step("sum(parking.free)", "C62")
            .validate(&inputs())
            .expect_err("unknown attribute");
        assert!(err.to_string().contains("`free` is not among"), "{err}");
        let err = step("median(parking.occupied)", "C62")
            .validate(&inputs())
            .expect_err("unknown function");
        assert!(err.to_string().contains("is not a function"), "{err}");
        assert!(step("sum(parking.occupied", "C62")
            .validate(&inputs())
            .is_err());
        assert!(step("avg(parking)", "C62").validate(&inputs()).is_err());
        assert!(step("1 + 2", "C62").validate(&inputs()).is_err());
        assert!(step(&"1+".repeat(300), "C62").validate(&inputs()).is_err());
    }

    #[test]
    fn units_must_agree_in_a_sum_and_with_the_declared_unit() {
        let mut ins = inputs();
        ins[0].units.insert("occupied".into(), "HUR".into());
        let err = step("sum(parking.occupied) + sum(stops.wait)", "MIN")
            .validate(&ins)
            .expect_err("hours plus minutes");
        assert!(err.to_string().contains("one unit"), "{err}");
        let err = step("avg(stops.wait)", "C62")
            .validate(&ins)
            .expect_err("minutes declared as a count");
        assert!(err.to_string().contains("is MIN, not C62"), "{err}");
        assert!(step("avg(stops.wait)", "MIN").validate(&ins).is_ok());
        assert!(step("sum(stops.wait) / sum(stops.wait) * 100", "P1")
            .validate(&ins)
            .is_ok());
        let err = step("count(stops)", "MIN")
            .validate(&ins)
            .expect_err("a count is no time");
        assert!(err.to_string().contains("C62 or P1"), "{err}");
        // A slot without a unit takes the declared one on trust.
        assert!(step("sum(parking.capacity)", "E50").validate(&ins).is_ok());
    }

    #[test]
    fn a_join_by_area_needs_a_key_per_source_among_its_attributes() {
        let mut s = step("sum(parking.occupied) / count(stops)", "C62");
        s.join.by = JoinBy::Area;
        assert!(s.validate(&inputs()).is_err());
        s.join.keys = BTreeMap::from([
            ("parking".into(), "district".into()),
            ("stops".into(), "zone".into()),
        ]);
        assert!(s.validate(&inputs()).is_err());
        s.join.keys.insert("stops".into(), "district".into());
        assert!(s.validate(&inputs()).is_ok());
        let mut one = inputs();
        one.truncate(1);
        assert!(s.validate(&one).is_err(), "one source is the PL-45 preset");
    }

    fn pages() -> BTreeMap<String, Vec<Value>> {
        BTreeMap::from([
            (
                "parking".to_owned(),
                vec![
                    json!({"id": "urn:ngsi-ld:ParkingSite:a", "occupied": {"type": "Property", "value": 30}, "capacity": {"type": "Property", "value": 40}, "district": {"type": "Property", "value": "Kallio"}}),
                    json!({"id": "urn:ngsi-ld:ParkingSite:b", "occupied": 10, "capacity": 60, "district": "Kamppi"}),
                    json!({"id": "urn:ngsi-ld:ParkingSite:c", "occupied": 5, "capacity": 10, "district": "Pasila"}),
                ],
            ),
            (
                "stops".to_owned(),
                vec![
                    json!({"id": "urn:ngsi-ld:TransitStop:1", "district": {"type": "Property", "value": "Kallio"}}),
                    json!({"id": "urn:ngsi-ld:TransitStop:2", "district": {"type": "Property", "value": "Kallio"}}),
                    json!({"id": "urn:ngsi-ld:TransitStop:3", "district": {"type": "Property", "value": "Kamppi"}}),
                ],
            ),
        ])
    }

    #[test]
    fn a_join_by_time_folds_every_row_of_the_run() {
        let s = step("sum(parking.occupied) / sum(parking.capacity) * 100", "P1");
        let expr = s.validate(&inputs()).expect("valid");
        let out = evaluate(&s, &expr, &pages()).expect("computes");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "parking-per-stop");
        assert!((out[0].value - 45.0 / 110.0 * 100.0).abs() < 1e-9);
    }

    #[test]
    fn a_join_by_area_writes_one_value_per_area_every_source_has() {
        let mut s = step("sum(parking.occupied) / count(stops)", "C62");
        s.join.by = JoinBy::Area;
        s.join.keys = BTreeMap::from([
            ("parking".into(), "district".into()),
            ("stops".into(), "district".into()),
        ]);
        let expr = s.validate(&inputs()).expect("valid");
        let out = evaluate(&s, &expr, &pages()).expect("computes");
        let got: Vec<(&str, f64)> = out.iter().map(|v| (v.name.as_str(), v.value)).collect();
        assert_eq!(
            got,
            [
                ("parking-per-stop-kallio", 15.0),
                ("parking-per-stop-kamppi", 10.0)
            ]
        );
    }

    #[test]
    fn a_join_by_entity_meets_on_an_id_and_a_relationship() {
        let mut s = step("sum(parking.occupied) - sum(sensors.count)", "C62");
        s.join.by = JoinBy::Entity;
        s.join.keys = BTreeMap::from([
            ("parking".into(), "id".into()),
            ("sensors".into(), "site".into()),
        ]);
        let mut ins = inputs();
        ins[1] = Input {
            name: "sensors".into(),
            entity_type: "Sensor".into(),
            attrs: vec!["site".into(), "count".into()],
            ..Input::default()
        };
        let expr = s.validate(&ins).expect("valid");
        let mut p = pages();
        p.remove("stops");
        p.insert(
            "sensors".into(),
            vec![json!({"id": "urn:ngsi-ld:Sensor:s1", "site": {"type": "Relationship", "object": "urn:ngsi-ld:ParkingSite:a"}, "count": 28})],
        );
        let out = evaluate(&s, &expr, &p).expect("computes");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key.as_deref(), Some("urn:ngsi-ld:ParkingSite:a"));
        assert_eq!(out[0].value, 2.0);
    }

    #[test]
    fn division_by_zero_an_empty_average_or_a_missing_source_computes_nothing() {
        let s = step("sum(parking.occupied) / count(stops)", "C62");
        let expr = s.validate(&inputs()).expect("valid");
        let mut p = pages();
        p.insert("stops".into(), vec![]);
        assert_eq!(
            evaluate(&s, &expr, &p),
            Err("division by zero: count(stops) is 0".to_owned())
        );
        let s = step("avg(stops.wait)", "MIN");
        let expr = s.validate(&inputs()).expect("valid");
        assert_eq!(
            evaluate(&s, &expr, &pages()),
            Err("avg(stops.wait) has no values".to_owned())
        );
        p.remove("stops");
        assert_eq!(
            evaluate(&s, &expr, &p),
            Err("input stops did not answer in this run".to_owned())
        );
    }

    #[test]
    fn a_key_slug_is_a_local_id_the_mapping_writes_the_same_way() {
        assert_eq!(slug("Kallio"), "kallio");
        assert_eq!(slug("Etu-Töölö / 2"), "etu-t-l-2");
        assert_eq!(slug("***"), "x");
        assert!(slug(&"a".repeat(90)).len() <= MAX_SLUG_CHARS);
    }

    #[test]
    fn the_compiled_mapping_quotes_every_name_and_reads_only_what_the_renderer_injects() {
        let s = step("sum(parking.occupied) / count(stops)", "C62");
        let expr = s.validate(&inputs()).expect("valid");
        let target = Target {
            pipeline_space: "helsinki",
            pipeline: "parking-per-stop",
        };
        let mapping = bloblang(&s, &expr, &inputs(), &target);
        assert!(mapping.contains("env(\"JC_ORG_DOMAIN\")"));
        let reads: BTreeSet<&str> = mapping
            .match_indices("env(\"")
            .map(|(at, _)| mapping[at + 5..].split('"').next().unwrap_or(""))
            .collect();
        assert_eq!(
            reads,
            BTreeSet::from([
                "JC_ORG_DOMAIN",
                "JC_SPACE",
                "JC_SOURCE_SPACE",
                "JC_SOURCE_SPACE_2"
            ]),
            "{mapping}"
        );
        assert!(mapping.contains("division by zero: count(stops) is 0"));
        assert!(mapping.contains("did not answer in this run"));
        assert!(wrap_mapping("parking").contains("\"input\": \"parking\""));
    }
}
