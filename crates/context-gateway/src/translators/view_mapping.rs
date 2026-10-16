//! The compiled IR of a Mapping an Endpoint's `spec.viewMappingRef` names (T-0175, EP-54,
//! DM-51, DM-52), read and checked when the Endpoint is loaded.
//!
//! Since T-3799 the broker's view inverts the caller's query and rebuilds every entity in the
//! target model (Antares `crates/antares-views`); the gateway only parses the IR when the
//! repository loads (`store.rs` leaves out, with a warning, an IR this build cannot run), and an
//! Endpoint with a mapping takes no write (`app.rs`). The IR is the JSON artifact Model Tools compiles from the
//! same specification as the Bloblang Bento runs; nothing here parses LinkML-Map.

use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The IR schema version this interpreter understands (DM-52).
///
/// Version 2 added the expression tree of a computed slot. A gateway runs exactly the version
/// it was built for, so a repository holding version-1 artifacts recompiles them.
pub const IR_VERSION: u64 = 2;

/// Why an IR document is not one this gateway can run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidIr {
    /// The document is not the JSON object an IR is.
    #[error("the mapping IR is not an object")]
    Malformed,
    /// A version this interpreter does not know, which is a Model Tools newer than this build.
    #[error("the mapping IR is version {0}, and this gateway interprets version {IR_VERSION}")]
    Version(u64),
    /// A slot entry the interpreter cannot make sense of.
    #[error("mapping IR slot {index}: {reason}")]
    Slot {
        /// Which entry.
        index: usize,
        /// What is wrong with it.
        reason: String,
    },
}

/// What one target slot is made of.
#[derive(Debug, Clone, PartialEq)]
pub enum Derivation {
    /// The target slot is the source slot under another name.
    Rename,
    /// A linear conversion of the source value: `value * factor + offset`.
    UnitConversion {
        /// The multiplier, source to target.
        factor: f64,
        /// The offset, added after the multiplication.
        offset: f64,
    },
    /// A finite value table with both directions precomputed.
    ValueMappings {
        /// Source value to target value.
        forward: BTreeMap<String, Value>,
        /// Target value back to source value; the compiler refuses a table without one.
        inverse: BTreeMap<String, Value>,
    },
    /// The source value read as another type.
    Cast(Cast),
    /// A value that depends on no source slot at all.
    Constant(Value),
    /// A computed slot: evaluated here from the attributes it reads, and never filtered,
    /// because an expression has no inverse (DM-51).
    Expr(Expression),
}

/// One node of the expression a computed slot carries (DM-51, DM-52).
///
/// The six forms are the whole of what the Bloblang compiler accepts, because both artifacts
/// are rendered from one validated parse; a node this interpreter does not know is an IR from
/// a Model Tools newer than this build, and is refused as such rather than skipped.
#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    /// The value of a source attribute, unwrapped from whichever NGSI-LD shape it arrived in.
    Slot(String),
    /// A literal.
    Constant(Value),
    /// Arithmetic over two numbers, or `+` over two strings.
    Arithmetic {
        /// Which operation.
        operator: Arithmetic,
        /// The left operand.
        left: Box<Expression>,
        /// The right operand.
        right: Box<Expression>,
    },
    /// A comparison, answering a boolean.
    Comparison {
        /// Which comparison.
        operator: Comparison,
        /// The left operand.
        left: Box<Expression>,
        /// The right operand.
        right: Box<Expression>,
    },
    /// `and` or `or` over booleans.
    Junction {
        /// True for `and`, false for `or`.
        all: bool,
        /// The operands.
        operands: Vec<Expression>,
    },
    /// `-operand`, over a number.
    Negate(Box<Expression>),
    /// `not operand`, over a boolean.
    Not(Box<Expression>),
}

/// The arithmetic the expression subset allows (DM-36).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arithmetic {
    /// `+`: addition, or concatenation when both operands are strings.
    Add,
    /// `-`.
    Subtract,
    /// `*`.
    Multiply,
    /// `/`, which answers a fraction even for two whole numbers, as Python and Bloblang do.
    Divide,
}

/// The comparisons the expression subset allows (DM-36).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    /// `==`.
    Equal,
    /// `!=`.
    NotEqual,
    /// `<`.
    Less,
    /// `<=`.
    LessOrEqual,
    /// `>`.
    Greater,
    /// `>=`.
    GreaterOrEqual,
}

/// The casts the IR uses, matching the reference engine exactly (DM-39).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cast {
    /// Python `int()`: truncates towards zero, refuses a non-integer literal.
    Integer,
    /// Python `float()`.
    Float,
    /// Python `str()`.
    String,
}

/// One target slot of the mapping.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    /// The name the view serves.
    pub target: String,
    /// The name the broker knows, absent for a slot that derives from no source slot.
    pub source: Option<String>,
    /// How the value is derived.
    pub derivation: Derivation,
    /// Whether a filter may name this slot, which is whether the derivation has an inverse.
    pub filterable: bool,
}

/// A compiled Mapping, ready to run in both directions.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewMapping {
    /// The class the broker holds.
    pub source_class: String,
    /// The class the view serves.
    pub target_class: String,
    slots: Vec<Slot>,
}

impl ViewMapping {
    /// Reads one compiled mapping IR document (DM-52).
    pub fn parse(document: &Value) -> Result<Self, InvalidIr> {
        let members = document.as_object().ok_or(InvalidIr::Malformed)?;
        let version = members
            .get("version")
            .and_then(Value::as_u64)
            .ok_or(InvalidIr::Malformed)?;
        if version != IR_VERSION {
            return Err(InvalidIr::Version(version));
        }

        let slots = members
            .get("slots")
            .and_then(Value::as_array)
            .ok_or(InvalidIr::Malformed)?
            .iter()
            .enumerate()
            .map(|(index, entry)| slot(index, entry))
            .collect::<Result<Vec<Slot>, InvalidIr>>()?;

        Ok(Self {
            source_class: text(members.get("sourceClass")).unwrap_or_default(),
            target_class: text(members.get("targetClass")).unwrap_or_default(),
            slots,
        })
    }
}
fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

/// One slot entry of the IR.
fn slot(index: usize, entry: &Value) -> Result<Slot, InvalidIr> {
    let refuse = |reason: &str| InvalidIr::Slot {
        index,
        reason: reason.to_owned(),
    };
    let members = entry.as_object().ok_or_else(|| refuse("not an object"))?;
    let target = text(members.get("target")).ok_or_else(|| refuse("names no target slot"))?;
    let source = text(members.get("source"));
    let kind = text(members.get("kind")).ok_or_else(|| refuse("names no kind"))?;

    let derivation = match kind.as_str() {
        "rename" => Derivation::Rename,
        "expr" => Derivation::Expr(expression(
            index,
            members
                .get("expression")
                .ok_or_else(|| refuse("a computed slot carries no expression"))?,
        )?),
        "constant" => Derivation::Constant(
            members
                .get("value")
                .cloned()
                .ok_or_else(|| refuse("a constant carries no value"))?,
        ),
        "unitConversion" => Derivation::UnitConversion {
            factor: members
                .get("factor")
                .and_then(Value::as_f64)
                .ok_or_else(|| refuse("a unit conversion carries no factor"))?,
            offset: members
                .get("offset")
                .and_then(Value::as_f64)
                .unwrap_or_default(),
        },
        "valueMappings" => Derivation::ValueMappings {
            forward: table(members.get("forward")).ok_or_else(|| refuse("no forward table"))?,
            inverse: table(members.get("inverse")).ok_or_else(|| refuse("no inverse table"))?,
        },
        "cast" => Derivation::Cast(match text(members.get("range")).as_deref() {
            Some("integer") => Cast::Integer,
            Some("float") => Cast::Float,
            Some("string") => Cast::String,
            other => {
                return Err(refuse(&format!(
                    "cast to `{}` is not one this gateway performs",
                    other.unwrap_or("?")
                )))
            }
        }),
        other => {
            return Err(refuse(&format!(
                "kind `{other}` is not one this gateway runs"
            )))
        }
    };

    if source.is_none() && !matches!(derivation, Derivation::Constant(_) | Derivation::Expr(_)) {
        return Err(refuse("names no source slot"));
    }
    Ok(Slot {
        target,
        source,
        // The compiler decides this, and a slot with no inverse must not become filterable
        // because this interpreter thinks it could manage.
        filterable: members
            .get("filterable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        derivation,
    })
}

/// One expression node of the IR (DM-52).
fn expression(index: usize, node: &Value) -> Result<Expression, InvalidIr> {
    let refuse = |reason: String| InvalidIr::Slot { index, reason };
    let members = node
        .as_object()
        .ok_or_else(|| refuse("an expression node is an object".to_owned()))?;

    if let Some(name) = text(members.get("slot")) {
        return Ok(Expression::Slot(name));
    }
    if let Some(value) = members.get("const") {
        return Ok(Expression::Constant(value.clone()));
    }
    if let Some(operator) = text(members.get("binary")) {
        let (left, right) = operands(index, members)?;
        let operator = match operator.as_str() {
            "+" => Arithmetic::Add,
            "-" => Arithmetic::Subtract,
            "*" => Arithmetic::Multiply,
            "/" => Arithmetic::Divide,
            other => return Err(refuse(format!("`{other}` is not an arithmetic operator"))),
        };
        return Ok(Expression::Arithmetic {
            operator,
            left: Box::new(left),
            right: Box::new(right),
        });
    }
    if let Some(operator) = text(members.get("compare")) {
        let (left, right) = operands(index, members)?;
        let operator = match operator.as_str() {
            "==" => Comparison::Equal,
            "!=" => Comparison::NotEqual,
            "<" => Comparison::Less,
            "<=" => Comparison::LessOrEqual,
            ">" => Comparison::Greater,
            ">=" => Comparison::GreaterOrEqual,
            other => return Err(refuse(format!("`{other}` is not a comparison operator"))),
        };
        return Ok(Expression::Comparison {
            operator,
            left: Box::new(left),
            right: Box::new(right),
        });
    }
    if let Some(operator) = text(members.get("boolean")) {
        let all = match operator.as_str() {
            "and" => true,
            "or" => false,
            other => return Err(refuse(format!("`{other}` is not a boolean operator"))),
        };
        let listed = members
            .get("operands")
            .and_then(Value::as_array)
            .ok_or_else(|| refuse("a boolean node lists no operands".to_owned()))?;
        let mut operands = Vec::with_capacity(listed.len());
        for operand in listed {
            operands.push(expression(index, operand)?);
        }
        return Ok(Expression::Junction { all, operands });
    }
    if let Some(operator) = text(members.get("unary")) {
        let operand = members
            .get("operand")
            .ok_or_else(|| refuse("a unary node carries no operand".to_owned()))?;
        let operand = Box::new(expression(index, operand)?);
        return match operator.as_str() {
            "-" => Ok(Expression::Negate(operand)),
            "not" => Ok(Expression::Not(operand)),
            other => Err(refuse(format!("`{other}` is not a unary operator"))),
        };
    }
    Err(refuse(
        "an expression node names no form this gateway evaluates".to_owned(),
    ))
}

/// The `left` and `right` of a two-operand expression node.
fn operands(
    index: usize,
    members: &Map<String, Value>,
) -> Result<(Expression, Expression), InvalidIr> {
    let side = |key: &str| {
        members.get(key).ok_or_else(|| InvalidIr::Slot {
            index,
            reason: format!("an operator node carries no `{key}`"),
        })
    };
    Ok((
        expression(index, side("left")?)?,
        expression(index, side("right")?)?,
    ))
}

fn table(value: Option<&Value>) -> Option<BTreeMap<String, Value>> {
    Some(
        value?
            .as_object()?
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}
