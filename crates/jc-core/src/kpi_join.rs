//! Indicators over several sources (PL-102…PL-105, T-4864).
//!
//! A `kpi` step names two to five sources by their `name`, a join (`time`, `entity`, `area`) and
//! a formula over aggregates of their attributes, `sum(parking.occupiedSpotNumber) /
//! count(stops)`. This module parses and checks the formula and compiles it to the Bloblang
//! mapping the runner folds the run's pages with; its tests hold the mapping to a reference
//! evaluator. A division by zero, an average over nothing or a source that did not answer
//! writes no value (PL-104).
//!
//! `prev(sum(parking.occupied), P7D)` reads a term one period back (PL-117): the run adds one
//! Temporal API read per source and period, and the term folds those rows as they stood then.

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
/// The most pairs of source and period one formula reads back (PL-117).
pub const MAX_EARLIER_READS: usize = 3;
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
/// An attribute a kpi step reads: an NGSI-LD short name, which the compiled mapping quotes too.
static ATTR_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[A-Za-z_][A-Za-z0-9_]{0,127}$").expect("valid regex"));
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
    /// Arithmetic over `sum|avg|min|max(input.attribute)` and `count(input)` (PL-103), a term
    /// read one period back as `prev(sum(input.attribute), P7D)` (PL-117).
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
        /// The period back a `prev` term reads, `P7D` (PL-117); the current page when absent.
        back: Option<String>,
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
            Self::Term {
                agg,
                input,
                attr,
                back,
            } => match (attr, back) {
                (Some(attr), Some(back)) => {
                    write!(f, "prev({}({input}.{attr}), {back})", agg.word())
                }
                (Some(attr), None) => write!(f, "{}({input}.{attr})", agg.word()),
                (None, _) => write!(f, "{}({input})", agg.word()),
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
                if word == "prev" {
                    return self.earlier();
                }
                self.term(&word)
            }
            Some(c) => Err(invalid(format!(
                "`{}` at character {} is not part of a formula",
                c as char,
                self.at + 1
            ))),
            None => Err(invalid("the formula ends too early")),
        }
    }

    /// The rest of a term once its function's name is read.
    fn term(&mut self, word: &str) -> Result<Expr> {
        let agg = Agg::parse(word).ok_or_else(|| {
            invalid(format!(
                "`{word}` is not a function: use sum, avg, min, max, count or prev"
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
        Ok(Expr::Term {
            agg,
            input,
            attr,
            back: None,
        })
    }

    /// `prev(term, period)` after its name (PL-117): one attribute term and a duration.
    fn earlier(&mut self) -> Result<Expr> {
        let shape = "prev() reads one attribute one period back, such as \
                     prev(sum(parking.occupied), P7D)";
        self.expect(b'(')?;
        let word = self.word();
        if word.is_empty() {
            return Err(invalid(shape));
        }
        let Expr::Term {
            agg,
            input,
            attr: Some(attr),
            ..
        } = self.term(&word)?
        else {
            return Err(invalid(shape));
        };
        self.expect(b',')?;
        let period = self.word();
        self.expect(b')')?;
        if !WINDOW_RE.is_match(&period) {
            return Err(invalid(format!(
                "`{period}` is not a period such as PT1H, P1D or P7D"
            )));
        }
        Ok(Expr::Term {
            agg,
            input,
            attr: Some(attr),
            back: Some(period),
        })
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

/// One term as the checks read it: source, attribute, aggregate and period back.
type TermRef<'e> = (&'e str, Option<&'e str>, Agg, Option<&'e str>);

fn terms<'e>(expr: &'e Expr, out: &mut Vec<TermRef<'e>>) {
    match expr {
        Expr::Num(_) => {}
        Expr::Term {
            agg,
            input,
            attr,
            back,
        } => out.push((input, attr.as_deref(), *agg, back.as_deref())),
        Expr::Neg(inner) => terms(inner, out),
        Expr::Bin(_, a, b) => {
            terms(a, out);
            terms(b, out);
        }
    }
}

/// One read back of a formula (PL-117): a source at one period, with the attributes it folds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Earlier {
    /// The source's `name`.
    pub input: String,
    /// The period back, `P7D`.
    pub period: String,
    /// The attributes the formula's `prev` terms read at that period.
    pub attrs: Vec<String>,
}

impl Earlier {
    /// The name this read's page goes by in a run and in [`evaluate`]; `@` is in no source name.
    pub fn page(&self) -> String {
        page_of(&self.input, &self.period)
    }

    /// The mapping's variable for this read's rows.
    fn var(&self) -> String {
        format!(
            "$was_{}_{}",
            self.input.replace('-', "_"),
            self.period.to_lowercase()
        )
    }
}

fn page_of(input: &str, period: &str) -> String {
    format!("{input}@{period}")
}

/// Every read back `expr` needs, each pair of source and period once, in the formula's order.
pub fn earlier_reads(expr: &Expr) -> Vec<Earlier> {
    let mut used = Vec::new();
    terms(expr, &mut used);
    let mut reads: Vec<Earlier> = Vec::new();
    for (input, attr, _, back) in used {
        let (Some(attr), Some(period)) = (attr, back) else {
            continue;
        };
        let read = match reads
            .iter_mut()
            .position(|r| r.input == input && r.period == period)
        {
            Some(at) => &mut reads[at],
            None => {
                reads.push(Earlier {
                    input: input.to_owned(),
                    period: period.to_owned(),
                    attrs: Vec::new(),
                });
                let last = reads.len() - 1;
                &mut reads[last]
            }
        };
        if !read.attrs.iter().any(|a| a == attr) {
            read.attrs.push(attr.to_owned());
        }
    }
    reads
}

/// A checked period's number and unit in seconds: `P7D` is 7 and 86400.
fn period_parts(period: &str) -> (u32, u32) {
    let digits: String = period.chars().filter(char::is_ascii_digit).collect();
    let unit = match (period.starts_with("PT"), period.ends_with('M')) {
        (false, _) => 86_400,
        (true, true) => 60,
        (true, false) => 3_600,
    };
    (digits.parse().unwrap_or(0), unit)
}

/// A checked period in seconds: `PT1H` is 3600.
pub fn period_seconds(period: &str) -> u32 {
    let (number, unit) = period_parts(period);
    number * unit
}

/// A period in words: `P7D` is `7 days`, `PT1H` is `1 hour`.
pub fn period_words(period: &str) -> String {
    let (number, unit) = period_parts(period);
    let unit = match unit {
        60 => "minute",
        3_600 => "hour",
        _ => "day",
    };
    let plural = if number == 1 { "" } else { "s" };
    format!("{number} {unit}{plural}")
}

/// Why a read back with no rows gives no value (PL-117), for the run's failed line and the
/// preview's problem.
pub fn no_history(period: &str) -> String {
    format!(
        "it has no history {} back, so there is no value; check that its space keeps this \
         attribute's history and that much of it exists",
        period_words(period)
    )
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
        // A term read back is a second read of the one source (PL-117).
        // A formula that does not parse is refused below, by its own reason.
        let earlier = parse(&self.formula).map_or(true, |expr| !earlier_reads(&expr).is_empty());
        let least = if earlier { 1 } else { MIN_SOURCES };
        if !(least..=MAX_SOURCES).contains(&inputs.len()) {
            return Err(Error::Invalid {
                field: "spec.sources".to_owned(),
                reason: format!(
                    "an indicator over several sources reads {MIN_SOURCES} to {MAX_SOURCES} of them; \
                     one source is the kpi preset of PL-45"
                ),
            });
        }
        source_names(inputs)?;
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
        self.validate_join(inputs)?;
        let expr = parse(&self.formula)?;
        formula_terms(&expr, inputs)?;
        self.check_earlier(&expr)?;
        self.check_unit(expr, inputs)
    }

    /// A read back goes with a join by time, a few times per formula (PL-117).
    /// ponytail: a past row carries no key of its own, so a change per area would match it to
    /// the current rows by id; the upgrade path is that match in `bloblang` and `evaluate`.
    fn check_earlier(&self, expr: &Expr) -> Result<()> {
        let reads = earlier_reads(expr);
        if !reads.is_empty() && self.join.by != JoinBy::Time {
            return Err(field(
                "join",
                "a value one period back is computed over all rows of the run: join by time",
            ));
        }
        if reads.len() > MAX_EARLIER_READS {
            return Err(invalid(format!(
                "a formula reads at most {MAX_EARLIER_READS} sources or periods back"
            )));
        }
        Ok(())
    }

    /// The join's keys name one read attribute (or id) of each source and nothing else.
    fn validate_join(&self, inputs: &[Input]) -> Result<()> {
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
        Ok(())
    }

    /// The formula's unit matches the declared unitCode.
    fn check_unit(&self, expr: Expr, inputs: &[Input]) -> Result<Expr> {
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

/// Each source has a unique DNS-1123 name and reads only attribute names.
fn source_names(inputs: &[Input]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for input in inputs {
        if let Some(attr) = input.attrs.iter().find(|a| !ATTR_RE.is_match(a)) {
            return Err(Error::Invalid {
                field: "spec.sources.query.attrs".to_owned(),
                reason: format!(
                    "`{attr}` of source `{}` is not an attribute name: letters, digits and _ (PL-103)",
                    input.name
                ),
            });
        }
        if input.name.is_empty() {
            return Err(Error::Invalid {
                field: "spec.sources.name".to_owned(),
                reason: "each source of a kpi step has a name the formula reads it by".to_owned(),
            });
        }
        names::validate_dns1123_label(&input.name)?;
        if !seen.insert(input.name.as_str()) {
            return Err(Error::Invalid {
                field: "spec.sources.name".to_owned(),
                reason: format!("two sources are named `{}`", input.name),
            });
        }
    }
    Ok(())
}

/// Each term of the formula reads a source and an attribute it reads.
fn formula_terms(expr: &Expr, inputs: &[Input]) -> Result<()> {
    let mut used = Vec::new();
    terms(expr, &mut used);
    for (name, attr, agg, _) in &used {
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
    Ok(())
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

/// A Bloblang string literal.
fn lit(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

/// The Bloblang query reading `attr` of row `e` as a key; the name is read as a quoted path.
fn key_query(attr: &str) -> String {
    let at = lit(attr);
    format!("e.get({at}).value.or(e.get({at}).object).or(e.get({at}))")
}

/// The numbers of `attr` on `rows`; a row read back holds a list of instances, newest first.
fn values(rows: &str, attr: &str, earlier: bool) -> String {
    let at = lit(attr);
    let member = if earlier {
        format!("e.get({at}).index(0).or(e.get({at}))")
    } else {
        format!("e.get({at})")
    };
    format!("{rows}.map_each(e -> {member}.value.or({member})).filter(v -> v.type() == \"number\")")
}

/// `expr` as a Bloblang query over the per-source row variables `rows(name)`.
fn query(expr: &Expr, rows: &dyn Fn(&str) -> String) -> String {
    match expr {
        Expr::Num(n) => format!("{n:?}"),
        Expr::Term {
            agg,
            input,
            attr,
            back,
        } => {
            let earlier = back.as_ref().map(|period| Earlier {
                input: input.clone(),
                period: period.clone(),
                attrs: Vec::new(),
            });
            let rows = earlier.as_ref().map_or_else(|| rows(input), Earlier::var);
            let Some(attr) = attr else {
                return format!("{rows}.length()");
            };
            let values = values(&rows, attr, earlier.is_some());
            let empty = lit(&format!("{expr} has no values"));
            let guarded = |fold: String| {
                format!("(if {values}.length() == 0 {{ throw({empty}) }} else {{ {fold} }})")
            };
            match agg {
                // A count or sum over nothing is 0 now; one period back it is no history.
                Agg::Count if earlier.is_none() => format!("{values}.length()"),
                Agg::Sum if earlier.is_none() => format!("{values}.sum()"),
                Agg::Count => guarded(format!("{values}.length()")),
                Agg::Sum => guarded(format!("{values}.sum()")),
                Agg::Avg => guarded(format!("{values}.sum() / {values}.length()")),
                Agg::Min | Agg::Max => guarded(format!("{values}.{}()", agg.word())),
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
    let reads = earlier_reads(expr);
    let names: Vec<String> = inputs
        .iter()
        .map(|i| lit(&i.name))
        .chain(reads.iter().map(|read| lit(&read.page())))
        .collect();
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
    for read in &reads {
        out.push_str(&format!(
            "let {} = $pages.filter(p -> p.input == {}).index(0).entities.or([])\n",
            read.var().trim_start_matches('$'),
            lit(&read.page())
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
        for read in &reads {
            text.push_str(&format!(
                " + {} + {}.length().string() + \" rows\"",
                lit(&format!(
                    "; {} {} back: ",
                    read.input,
                    period_words(&read.period)
                )),
                read.var()
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

// The reference evaluator the compiled mapping is held to (the example's bento test asserts
// the same values) and the Portal's preview computes with (PL-105).
/// The number an attribute holds on a row: a Property's `value`, a keyValues number, or the first
/// of a temporal row's instances, newest first.
fn number(row: &Value, attr: &str) -> Option<f64> {
    let member = row.get(attr)?;
    let member = member
        .as_array()
        .and_then(|instances| instances.first())
        .unwrap_or(member);
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
        Expr::Term {
            agg,
            input,
            attr,
            back,
        } => {
            let page = back.as_ref().map(|period| page_of(input, period));
            let page = page.as_deref().unwrap_or(input);
            let rows = rows.get(page).map(Vec::as_slice).unwrap_or(&[]);
            let Some(attr) = attr else {
                return Ok(rows.len() as f64);
            };
            let values: Vec<f64> = rows.iter().filter_map(|row| number(row, attr)).collect();
            match agg {
                _ if back.is_some() && values.is_empty() => {
                    return Err(format!("{expr} has no values"))
                }
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
    for (name, _, _, back) in &used {
        let page = back.map_or_else(|| (*name).to_owned(), |period| page_of(name, period));
        if !pages.contains_key(&page) {
            return Err(format!("input {page} did not answer in this run"));
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
        let keys: BTreeSet<String> = rows_of(pages, name)
            .iter()
            .filter_map(|r| key(r, attr))
            .collect();
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
                    let rows = rows_of(pages, name)
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

/// A source's page; [`evaluate`] checked first that every named source answered.
fn rows_of<'p>(pages: &'p BTreeMap<String, Vec<Value>>, name: &str) -> &'p [Value] {
    pages.get(name).map(Vec::as_slice).unwrap_or(&[])
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
    fn a_term_read_back_parses_and_names_its_read() {
        let expr =
            parse("sum(parking.occupied) - prev(sum(parking.occupied), P7D)").expect("parses");
        assert_eq!(
            expr.to_string(),
            "(sum(parking.occupied) - prev(sum(parking.occupied), P7D))"
        );
        let expr = parse(
            "prev(avg(parking.occupied), P7D) + prev(max(parking.capacity), P7D) + prev(count(stops.wait), PT1H)",
        )
        .expect("parses");
        let reads = earlier_reads(&expr);
        assert_eq!(reads.len(), 2);
        assert_eq!(reads[0].page(), "parking@P7D");
        assert_eq!(reads[0].attrs, ["occupied", "capacity"]);
        assert_eq!(reads[1].page(), "stops@PT1H");
    }

    #[test]
    fn a_read_back_of_anything_but_one_attribute_term_is_refused() {
        for (formula, reason) in [
            ("prev(count(parking), P7D)", "prev() reads one attribute"),
            ("prev(sum(parking.occupied) + 1, P7D)", "expected `,`"),
            ("prev(1, P7D)", "not a function"),
            ("prev(sum(parking.occupied))", "expected `,`"),
            ("prev(sum(parking.occupied), 7)", "not a period"),
            ("prev(sum(parking.occupied), P1Y)", "not a period"),
            ("prev(median(parking.occupied), P7D)", "not a function"),
            (
                "prev(prev(sum(parking.occupied), P1D), P7D)",
                "not a function",
            ),
        ] {
            let err = step(formula, "C62").validate(&inputs()).expect_err(formula);
            assert!(err.to_string().contains(reason), "{formula}: {err}");
        }
        let err = step("prev(sum(cars.occupied), P7D)", "C62")
            .validate(&inputs())
            .expect_err("unknown source");
        assert!(err.to_string().contains("`cars` is not a source"), "{err}");
        let err = step("prev(sum(parking.free), P7D)", "C62")
            .validate(&inputs())
            .expect_err("an attribute the source does not read");
        assert!(err.to_string().contains("`free` is not among"), "{err}");
    }

    #[test]
    fn a_read_back_takes_one_source_a_join_by_time_and_a_few_reads() {
        let one = &inputs()[..1];
        let change = "sum(parking.occupied) - prev(sum(parking.occupied), P7D)";
        step(change, "C62").validate(one).expect("one source");
        let err = step("sum(parking.occupied)", "C62")
            .validate(one)
            .expect_err("one source with nothing read back");
        assert!(err.to_string().contains("reads 2 to 5"), "{err}");

        let mut by_area = step(change, "C62");
        by_area.join.by = JoinBy::Area;
        by_area.join.keys = BTreeMap::from([("parking".into(), "district".into())]);
        let err = by_area.validate(one).expect_err("a join by area");
        assert!(err.to_string().contains("join by time"), "{err}");

        let four = "prev(sum(parking.occupied), P1D) + prev(sum(parking.occupied), P2D) \
                    + prev(sum(parking.occupied), P3D) + prev(sum(parking.occupied), P4D)";
        let err = step(four, "C62").validate(one).expect_err("four reads");
        assert!(err.to_string().contains("at most 3"), "{err}");
    }

    #[test]
    fn a_term_read_back_keeps_its_unit() {
        let mut ins = inputs();
        ins[0].units.insert("occupied".into(), "HUR".into());
        let err = step("sum(stops.wait) - prev(sum(parking.occupied), P7D)", "MIN")
            .validate(&ins)
            .expect_err("minutes minus hours");
        assert!(err.to_string().contains("one unit"), "{err}");
        step("prev(sum(stops.wait), P7D) - sum(stops.wait)", "MIN")
            .validate(&ins)
            .expect("one unit");
    }

    #[test]
    fn a_period_reads_in_words() {
        assert_eq!(period_words("P7D"), "7 days");
        assert_eq!(period_words("P1D"), "1 day");
        assert_eq!(period_words("PT1H"), "1 hour");
        assert_eq!(period_words("PT15M"), "15 minutes");
        assert!(no_history("P7D").contains("7 days back"));
        assert_eq!(period_seconds("P7D"), 604_800);
        assert_eq!(period_seconds("PT15M"), 900);
    }

    /// A temporal row as the broker answers one on dev: each attribute a list of instances.
    fn back(value: f64) -> Value {
        json!({ "id": "urn:a", "type": "ParkingSite",
                "occupied": [{ "type": "Property", "value": value }] })
    }

    #[test]
    fn a_change_folds_the_current_page_minus_the_page_read_back() {
        let s = step(
            "sum(parking.occupied) - prev(sum(parking.occupied), P7D)",
            "C62",
        );
        let ins = &inputs()[..1];
        let expr = s.validate(ins).expect("valid");
        let pages = BTreeMap::from([
            (
                "parking".to_owned(),
                vec![json!({ "id": "urn:a", "occupied": { "type": "Property", "value": 30 } })],
            ),
            ("parking@P7D".to_owned(), vec![back(10.0), back(12.0)]),
        ]);
        let computed = evaluate(&s, &expr, &pages).expect("computed");
        assert_eq!(computed[0].value, 8.0);
        // One instance can come as a bare object too (T-5432).
        let mut one = pages.clone();
        one.insert(
            "parking@P7D".to_owned(),
            vec![json!({ "occupied": { "type": "Property", "value": 5 } })],
        );
        assert_eq!(evaluate(&s, &expr, &one).expect("computed")[0].value, 25.0);
    }

    #[test]
    fn no_history_is_no_value_never_a_zero() {
        let s = step(
            "sum(parking.occupied) - prev(sum(parking.occupied), P7D)",
            "C62",
        );
        let expr = s.validate(&inputs()[..1]).expect("valid");
        let mut pages = BTreeMap::from([(
            "parking".to_owned(),
            vec![json!({ "occupied": { "type": "Property", "value": 30 } })],
        )]);
        let err = evaluate(&s, &expr, &pages).expect_err("the read did not answer");
        assert!(err.contains("input parking@P7D did not answer"), "{err}");
        pages.insert("parking@P7D".to_owned(), Vec::new());
        let err = evaluate(&s, &expr, &pages).expect_err("no rows back");
        assert!(err.contains("has no values"), "{err}");
    }

    #[test]
    fn the_compiled_mapping_guards_a_read_back_and_says_what_it_read() {
        let s = step(
            "sum(parking.occupied) - prev(sum(parking.occupied), P7D)",
            "C62",
        );
        let ins = &inputs()[..1];
        let expr = s.validate(ins).expect("valid");
        let target = Target {
            pipeline_space: "helsinki",
            pipeline: "change",
        };
        let mapping = bloblang(&s, &expr, ins, &target);
        assert!(
            mapping.contains("\"parking\", \"parking@P7D\""),
            "{mapping}"
        );
        assert!(mapping.contains("let was_parking_p7d = "), "{mapping}");
        assert!(
            mapping.contains("throw(\"prev(sum(parking.occupied), P7D) has no values\")"),
            "{mapping}"
        );
        assert!(mapping.contains("; parking 7 days back: "), "{mapping}");
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

    /// Review 2026-10-21 (integrator): an attribute or key is read into the compiled mapping, so
    /// Bloblang written as an attribute name must be refused, and a hyphenated name too.
    #[test]
    fn bloblang_written_as_an_attribute_or_key_is_refused() {
        let mut s = step("sum(parking.occupied) / count(stops)", "C62");
        s.join.by = JoinBy::Area;
        s.join.keys = BTreeMap::from([
            ("parking".into(), "x.or(env(\"X\"))".into()),
            ("stops".into(), "district".into()),
        ]);
        let mut ins = inputs();
        ins[0].attrs.push("x.or(env(\"X\"))".into());
        let err = s.validate(&ins).expect_err("an expression as a key");
        assert!(
            err.to_string().contains("is not an attribute name"),
            "{err}"
        );

        let mut ins = inputs();
        ins[0].attrs.push("my-attr".into());
        let err = step("sum(parking.my-attr)", "C62")
            .validate(&ins)
            .expect_err("a hyphen reads as a subtraction");
        assert!(
            err.to_string().contains("is not an attribute name"),
            "{err}"
        );
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
