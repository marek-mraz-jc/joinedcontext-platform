//! Serving one space as somebody else's model (T-0175, EP-54, DM-51, DM-52).
//!
//! An Endpoint with `spec.viewMappingRef` publishes the *target* model of a Mapping. The
//! broker knows only the source model, so the gateway stands in the middle and runs the
//! Mapping's compiled IR in both directions: the caller's query is inverted on the way out,
//! and every entity is rebuilt on the way back.
//!
//! The IR is the JSON artifact Model Tools compiles from the same specification as the
//! Bloblang Bento runs, and DM-39's golden tests run against both, so the two executors
//! cannot answer differently. Nothing here parses LinkML-Map: this module only interprets an
//! IR that was already reduced to the invertible subset DM-51 allows.
//!
//! A computed (`expr`) slot is the one entry that is neither inverted nor read straight from
//! the broker: the IR carries its expression as a typed tree, and this module evaluates that
//! tree over the attributes the entity already has. Nothing here parses a language, for the
//! same reason the rest of the IR carries precomputed tables rather than rules.
//!
//! A view is read only. A mapping is invertible per slot, not per entity — a required source
//! slot no target slot derives has no value to reconstruct, and a constant or an `expr` slot
//! has none at all — so a write in the target model is refused rather than half-inverted.

use antares_ql::{CmpOp, QNode, QPath, QValue};
use jc_core::ProblemDetails;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The IR schema version this interpreter understands (DM-52).
///
/// Version 2 added the expression tree of a computed slot. A gateway runs exactly the version
/// it was built for, so a repository holding version-1 artifacts recompiles them.
pub const IR_VERSION: u64 = 2;

/// The members of an entity that belong to NGSI-LD rather than to a model, and travel
/// through a view untouched. `type` is not one of them: a view answers with the target class.
const STRUCTURAL: &[&str] = &["id", "@id", "@context", "scope"];

/// The members the broker generates, which a view keeps for the same reason a projection does
/// (EP-71): they say when the data was written and who answered, not what it contains.
const SYSTEM: &[&str] = &["createdAt", "modifiedAt", "deletedAt", "expiresAt"];

/// The `type` of the discovery answers (CIM 009 5.2.24 to 5.2.27), which name types and
/// attributes rather than carry them.
const VOCABULARY: &[&str] = &[
    "EntityTypeList",
    "EntityType",
    "EntityTypeInfo",
    "AttributeList",
    "Attribute",
];

/// A type no broker stores: what a type selection of a class the view does not serve
/// becomes, so it selects nothing (T-3672).
const NOT_SERVED: &str = "urn:joinedcontext:view:type-not-served";

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

impl Expression {
    /// The source slots this expression reads, appended to `into` if not already there.
    ///
    /// A caller who asks for a computed attribute is asking for these: without them the
    /// broker's answer has nothing to compute from, and the attribute would be absent for a
    /// reason the caller never sees (DM-51).
    fn reads(&self, into: &mut Vec<String>) {
        match self {
            Expression::Slot(name) => {
                if !into.iter().any(|seen| seen == name) {
                    into.push(name.clone());
                }
            }
            Expression::Constant(_) => {}
            Expression::Arithmetic { left, right, .. }
            | Expression::Comparison { left, right, .. } => {
                left.reads(into);
                right.reads(into);
            }
            Expression::Junction { operands, .. } => {
                for operand in operands {
                    operand.reads(into);
                }
            }
            Expression::Negate(operand) | Expression::Not(operand) => operand.reads(into),
        }
    }

    /// The value this expression has for one entity, or `None` where it has none.
    ///
    /// `None` is an attribute the answer leaves out: an input the broker did not send, or
    /// operands the expression cannot combine, such as a string added to a number. NGSI-LD
    /// has no null attribute and a view must not invent a value, which is the rule a cast the
    /// gateway cannot perform already follows.
    fn evaluate(&self, members: &Map<String, Value>) -> Option<Value> {
        match self {
            Expression::Slot(name) => members.get(name).map(plain_value),
            Expression::Constant(value) => Some(value.clone()),
            Expression::Arithmetic {
                operator,
                left,
                right,
            } => arithmetic(
                *operator,
                &left.evaluate(members)?,
                &right.evaluate(members)?,
            ),
            Expression::Comparison {
                operator,
                left,
                right,
            } => compare(
                *operator,
                &left.evaluate(members)?,
                &right.evaluate(members)?,
            ),
            // Every operand is evaluated, rather than stopping at the one that settles the
            // answer: an operand that cannot be evaluated is an expression the view cannot
            // serve, and hiding that behind a short circuit would make the attribute appear
            // or vanish depending on the order somebody wrote the terms in.
            Expression::Junction { all, operands } => {
                let mut answer = *all;
                for operand in operands {
                    let value = operand.evaluate(members)?;
                    let value = value.as_bool()?;
                    answer = if *all {
                        answer && value
                    } else {
                        answer || value
                    };
                }
                Some(Value::Bool(answer))
            }
            Expression::Negate(operand) => match operand.evaluate(members)? {
                Value::Number(number) => match number.as_i64() {
                    Some(whole) => Some(Value::from(-whole)),
                    None => finite(-number.as_f64()?),
                },
                _ => None,
            },
            Expression::Not(operand) => Some(Value::Bool(!operand.evaluate(members)?.as_bool()?)),
        }
    }
}

/// One attribute's value, whatever NGSI-LD shape it arrived in: the read side of `map_value`.
fn plain_value(attribute: &Value) -> Value {
    let Some(members) = attribute.as_object() else {
        return attribute.clone();
    };
    ["value", "object"]
        .into_iter()
        .find_map(|key| members.get(key))
        .cloned()
        .unwrap_or_else(|| attribute.clone())
}

/// `left <operator> right`, or `None` where the two do not combine.
fn arithmetic(operator: Arithmetic, left: &Value, right: &Value) -> Option<Value> {
    // Concatenation is the one operation over strings, and the one place `+` is not addition.
    if let (Some(head), Some(tail)) = (left.as_str(), right.as_str()) {
        return match operator {
            Arithmetic::Add => Some(Value::String(format!("{head}{tail}"))),
            _ => None,
        };
    }
    // Two whole numbers stay whole, as they do in Python and in Bloblang, so a count does not
    // come back from a view with a decimal point it never had.
    if let (Some(left), Some(right)) = (left.as_i64(), right.as_i64()) {
        return match operator {
            Arithmetic::Add => left.checked_add(right).map(Value::from),
            Arithmetic::Subtract => left.checked_sub(right).map(Value::from),
            Arithmetic::Multiply => left.checked_mul(right).map(Value::from),
            Arithmetic::Divide => divide(left as f64, right as f64),
        };
    }
    let (left, right) = (left.as_f64()?, right.as_f64()?);
    match operator {
        Arithmetic::Add => finite(left + right),
        Arithmetic::Subtract => finite(left - right),
        Arithmetic::Multiply => finite(left * right),
        Arithmetic::Divide => divide(left, right),
    }
}

/// A quotient, or `None` for a division by zero, which has no value to serve.
fn divide(left: f64, right: f64) -> Option<Value> {
    if right == 0.0 {
        return None;
    }
    finite(left / right)
}

/// A JSON number, or `None` where the result is not one: JSON has no NaN and no infinity.
fn finite(value: f64) -> Option<Value> {
    serde_json::Number::from_f64(value).map(Value::Number)
}

/// `left <operator> right`, or `None` where the two are not comparable.
fn compare(operator: Comparison, left: &Value, right: &Value) -> Option<Value> {
    if let (Some(left), Some(right)) = (left.as_bool(), right.as_bool()) {
        return match operator {
            Comparison::Equal => Some(Value::Bool(left == right)),
            Comparison::NotEqual => Some(Value::Bool(left != right)),
            // Ordering two booleans is not a comparison this subset defines.
            _ => None,
        };
    }
    let ordering = match (left.as_str(), right.as_str()) {
        (Some(left), Some(right)) => left.cmp(right),
        _ => left.as_f64()?.partial_cmp(&right.as_f64()?)?,
    };
    Some(Value::Bool(match operator {
        Comparison::Equal => ordering.is_eq(),
        Comparison::NotEqual => ordering.is_ne(),
        Comparison::Less => ordering.is_lt(),
        Comparison::LessOrEqual => ordering.is_le(),
        Comparison::Greater => ordering.is_gt(),
        Comparison::GreaterOrEqual => ordering.is_ge(),
    }))
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

    /// The slot a target name refers to.
    pub fn by_target(&self, name: &str) -> Option<&Slot> {
        self.slots.iter().find(|slot| slot.target == name)
    }

    /// The source name a target name reads, when the derivation has one.
    pub fn source_of(&self, target: &str) -> Option<&str> {
        self.by_target(target)?.source.as_deref()
    }

    /// Rebuilds one entity, or a whole answer, in the target model (EP-54).
    pub fn translate(&self, body: &mut Value) {
        match body {
            Value::Array(entities) => {
                for entity in entities {
                    self.translate_entity(entity);
                }
            }
            entity => self.translate_entity(entity),
        }
    }

    /// Rebuilds one entity in the target model.
    ///
    /// Everything the mapping does not derive is dropped: a view serves the target model, and
    /// an attribute that leaks through under its source name is one no target schema
    /// describes.
    pub fn translate_entity(&self, entity: &mut Value) {
        let Some(members) = entity.as_object() else {
            return;
        };
        if let Some(Value::String(kind)) = members.get("type") {
            if VOCABULARY.contains(&kind.as_str()) {
                self.translate_vocabulary(entity);
                return;
            }
        }
        // Not an entity: a problem document.
        if !members.contains_key("id") && !members.contains_key("@id") {
            return;
        }

        let mut translated = Map::new();
        for name in STRUCTURAL.iter().chain(SYSTEM.iter()) {
            if let Some(value) = members.get(*name) {
                translated.insert((*name).to_owned(), value.clone());
            }
        }
        translated.insert("type".to_owned(), Value::String(self.target_class.clone()));

        let normalized = is_normalized(members);
        for slot in &self.slots {
            if let Derivation::Constant(value) = &slot.derivation {
                translated.insert(slot.target.clone(), produced(value.clone(), normalized));
                continue;
            }
            if let Derivation::Expr(expression) = &slot.derivation {
                // A computed slot reads the attributes the broker sent rather than one source
                // attribute of its own; where it has no value it is left out (DM-51).
                if let Some(value) = expression.evaluate(members) {
                    translated.insert(slot.target.clone(), produced(value, normalized));
                }
                continue;
            }
            let Some(source) = slot.source.as_deref() else {
                continue;
            };
            let Some(attribute) = members.get(source) else {
                continue;
            };
            translated.insert(slot.target.clone(), forward(&slot.derivation, attribute));
        }
        *entity = Value::Object(translated);
    }

    /// A discovery answer (5.7.5 to 5.7.10) in the target model (T-3672): the source class is
    /// the target class and any other type is not served here; an attribute list holds the
    /// target names the listed source attributes produce, the constants, and each computed
    /// slot whose every input is listed.
    fn translate_vocabulary(&self, document: &mut Value) {
        let Some(members) = document.as_object_mut() else {
            return;
        };
        let class = |name: &Value| {
            (name.as_str() == Some(self.source_class.as_str()))
                .then(|| Value::String(self.target_class.clone()))
        };
        for key in ["typeList", "typeNames"] {
            if let Some(Value::Array(names)) = members.get_mut(key) {
                *names = names.iter().filter_map(class).collect();
            }
        }
        for key in ["typeName", "id"] {
            if let Some(name) = members.get(key).and_then(class) {
                members.insert(key.to_owned(), name);
            }
        }
        for key in ["attributeList", "attributeNames"] {
            if let Some(Value::Array(names)) = members.get_mut(key) {
                let listed: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
                *names = self
                    .targets_of(&listed)
                    .into_iter()
                    .map(Value::String)
                    .collect();
            }
        }
        if let Some(Value::String(name)) = members.get("attributeName") {
            if let Some(target) = self.targets_of(&[name.as_str()]).into_iter().next() {
                members.insert("attributeName".to_owned(), Value::String(target));
            }
        }
    }

    /// The target slots that the source attributes `listed` produce, in slot order.
    fn targets_of(&self, listed: &[&str]) -> Vec<String> {
        self.slots
            .iter()
            .filter(|slot| match &slot.derivation {
                Derivation::Constant(_) => true,
                Derivation::Expr(expression) => {
                    let mut reads = Vec::new();
                    expression.reads(&mut reads);
                    reads.iter().all(|read| listed.contains(&read.as_str()))
                }
                _ => slot
                    .source
                    .as_deref()
                    .is_some_and(|source| listed.contains(&source)),
            })
            .map(|slot| slot.target.clone())
            .collect()
    }

    /// A type selection (4.17) in the source model (T-3672): the target class is the source
    /// class, and every other type is one the view does not serve, so it becomes a type no
    /// broker stores and selects nothing, in an `;` and in an `|` alike.
    pub fn invert_type(&self, selection: &str) -> String {
        let mut out = String::with_capacity(selection.len());
        let mut name = String::new();
        let flush = |name: &mut String, out: &mut String| {
            let trimmed = name.trim();
            if trimmed == self.target_class {
                out.push_str(&self.source_class);
            } else if !trimmed.is_empty() {
                out.push_str(NOT_SERVED);
            }
            name.clear();
        };
        for character in selection.chars() {
            if matches!(character, ',' | '|' | ';' | '(' | ')') {
                flush(&mut name, &mut out);
                out.push(character);
            } else {
                name.push(character);
            }
        }
        flush(&mut name, &mut out);
        out
    }

    /// The `attrs` a caller asked for, in the source model's names (DM-51).
    ///
    /// A name the mapping does not define is a bad request rather than a silent omission: the
    /// caller asked for something this view does not serve, and answering as if they had not
    /// asked is how a client concludes the data is missing.
    pub fn invert_attrs(&self, attrs: &str) -> Result<String, Box<ProblemDetails>> {
        let mut sources = Vec::new();
        for asked in attrs.split(',').map(str::trim).filter(|a| !a.is_empty()) {
            let Some(slot) = self.by_target(asked) else {
                return Err(Box::new(unknown(asked)));
            };
            // A constant is produced here, so the broker is not asked for it. A computed
            // slot is produced here too, but not out of nothing: the broker has to send the
            // attributes its expression reads, or there is nothing to compute (DM-51).
            match &slot.derivation {
                Derivation::Expr(expression) => expression.reads(&mut sources),
                _ => {
                    if let Some(source) = &slot.source {
                        sources.push(source.clone());
                    }
                }
            }
        }
        Ok(sources.join(","))
    }

    /// The `geoproperty` a caller named, in the source model's name.
    pub fn invert_geo_property(&self, name: &str) -> Result<String, Box<ProblemDetails>> {
        match self.by_target(name) {
            Some(slot) if slot.filterable => {
                Ok(slot.source.clone().unwrap_or_else(|| slot.target.clone()))
            }
            Some(_) => Err(Box::new(not_filterable(name))),
            None => Err(Box::new(unknown(name))),
        }
    }

    /// The caller's `q`, rewritten into the source model (DM-51).
    ///
    /// Attribute names become source names and a compared value goes back through the slot's
    /// own inverse, so a filter written in the target model reaches the broker as a filter
    /// the broker's own data satisfies. The rewrite runs on the parsed query, so a separator
    /// inside a quoted value stays in the value (T-3672).
    pub fn invert_q(&self, filter: &str) -> Result<String, Box<ProblemDetails>> {
        let node = antares_ql::parse_q(filter).map_err(|error| {
            Box::new(ProblemDetails::bad_request().with_detail(error.to_string()))
        })?;
        Ok(self.invert_node(node)?.to_string())
    }

    fn invert_node(&self, node: QNode) -> Result<QNode, Box<ProblemDetails>> {
        let all = |nodes: Vec<QNode>| -> Result<Vec<QNode>, Box<ProblemDetails>> {
            nodes.into_iter().map(|n| self.invert_node(n)).collect()
        };
        match node {
            QNode::And(nodes) => all(nodes).map(QNode::And),
            QNode::Or(nodes) => all(nodes).map(QNode::Or),
            QNode::Exists { path, negated } => Ok(QNode::Exists {
                path: self.source_path(&path)?.0,
                negated,
            }),
            QNode::Cmp { path, op, value } => {
                let (source, slot) = self.source_path(&path)?;
                invert_comparison(&slot.derivation, source, op, value)
                    .ok_or_else(|| Box::new(not_comparable(&slot.target, op)))
            }
        }
    }

    /// The source attribute a term's path names, and its slot. Only a plain attribute name
    /// is mapped: a sub-attribute, a language or a linked entity behind it is not.
    fn source_path(&self, path: &QPath) -> Result<(QPath, &Slot), Box<ProblemDetails>> {
        let [attribute] = path.path.as_slice() else {
            return Err(Box::new(unknown(&path.to_string())));
        };
        if !path.links.is_empty() || path.bracket.is_some() {
            return Err(Box::new(unknown(&path.to_string())));
        }
        let Some(slot) = self.by_target(attribute) else {
            return Err(Box::new(unknown(attribute)));
        };
        if !slot.filterable {
            return Err(Box::new(not_filterable(attribute)));
        }
        let source = slot.source.clone().unwrap_or_else(|| slot.target.clone());
        let path = QPath {
            links: Vec::new(),
            path: vec![source],
            bracket: None,
        };
        Ok((path, slot))
    }
}

/// One comparison in the source model, or `None` when the derivation has no inverse for it.
fn invert_comparison(
    derivation: &Derivation,
    path: QPath,
    op: CmpOp,
    value: QValue,
) -> Option<QNode> {
    match derivation {
        Derivation::ValueMappings { forward, .. } => {
            // A table has no order: only equality inverts, to every source value that maps
            // to an asked one.
            if !matches!(op, CmpOp::Eq | CmpOp::Ne) {
                return None;
            }
            let asked = match value {
                QValue::List(items) => items,
                QValue::Range(..) => return None,
                one => vec![one],
            };
            let mut sources = Vec::new();
            for target in &asked {
                let text = scalar_text(target)?;
                for (source, produced) in forward {
                    if plain(produced) == text {
                        sources.push(source_value(source));
                    }
                }
            }
            Some(match (sources.len(), op) {
                // nothing maps to it: `==` holds for no entity, `!=` for each that has it
                (0, CmpOp::Eq) => QNode::And(vec![
                    QNode::Exists {
                        path: path.clone(),
                        negated: false,
                    },
                    QNode::Exists {
                        path,
                        negated: true,
                    },
                ]),
                (0, _) => QNode::Exists {
                    path,
                    negated: false,
                },
                (1, _) => QNode::Cmp {
                    path,
                    op,
                    value: sources.remove(0),
                },
                _ => QNode::Cmp {
                    path,
                    op,
                    value: QValue::List(sources),
                },
            })
        }
        Derivation::UnitConversion { factor, offset } if *factor != 0.0 => {
            let back = |v: QValue| match v {
                QValue::Num(n) => QValue::Num(rounded((n - offset) / factor)),
                other => other,
            };
            let value = match value {
                QValue::List(items) => QValue::List(items.into_iter().map(back).collect()),
                // a negative factor reverses the order, so the ends trade places
                QValue::Range(lo, hi) if *factor < 0.0 => {
                    QValue::Range(Box::new(back(*hi)), Box::new(back(*lo)))
                }
                QValue::Range(lo, hi) => QValue::Range(Box::new(back(*lo)), Box::new(back(*hi))),
                one => back(one),
            };
            let op = match (op, *factor < 0.0) {
                (CmpOp::Gt, true) => CmpOp::Lt,
                (CmpOp::Ge, true) => CmpOp::Le,
                (CmpOp::Lt, true) => CmpOp::Gt,
                (CmpOp::Le, true) => CmpOp::Ge,
                (op, _) => op,
            };
            Some(QNode::Cmp { path, op, value })
        }
        _ => Some(QNode::Cmp { path, op, value }),
    }
}

/// A value table's source key as the scalar the broker stores: the table keys a number or a
/// boolean by its text, as `plain` writes it.
fn source_value(key: &str) -> QValue {
    match serde_json::from_str::<Value>(key) {
        Ok(Value::Number(number)) => number
            .as_f64()
            .map_or_else(|| QValue::Str(key.to_owned()), QValue::Num),
        Ok(Value::Bool(flag)) => QValue::Bool(flag),
        _ => QValue::Str(key.to_owned()),
    }
}

/// A scalar of a `q` as the text a value table is keyed by.
fn scalar_text(value: &QValue) -> Option<String> {
    match value {
        QValue::Str(text) => Some(text.clone()),
        QValue::Num(_) | QValue::Bool(_) => Some(value.to_string()),
        QValue::List(_) | QValue::Range(..) => None,
    }
}

/// A converted number as the shortest text that round-trips would read, so an inverted
/// filter reads like one a person would have written.
fn rounded(value: f64) -> f64 {
    render(value).parse().unwrap_or(value)
}

/// `pick` and `omit` (4.21) on a translated answer: they name target members, which the
/// broker does not hold, so they are applied here, to each entity, after the translation.
pub fn project(body: &mut Value, pick: Option<&str>, omit: Option<&str>) {
    let names = |list: Option<&str>| -> Option<Vec<String>> {
        list.map(|list| list.split(',').map(|name| name.trim().to_owned()).collect())
    };
    let (pick, omit) = (names(pick), names(omit));
    let mut one = |entity: &mut Value| {
        let Some(members) = entity.as_object_mut() else {
            return;
        };
        if let Some(pick) = &pick {
            members.retain(|key, _| key == "@context" || pick.contains(key));
        }
        if let Some(omit) = &omit {
            members.retain(|key, _| !omit.contains(key));
        }
    };
    match body {
        Value::Array(entities) => entities.iter_mut().for_each(&mut one),
        entity => one(entity),
    }
}

/// The target value one source value becomes.
fn forward(derivation: &Derivation, attribute: &Value) -> Value {
    match derivation {
        // A computed or constant slot never reaches here: `translate_entity` produces it
        // before it looks for a source attribute.
        Derivation::Rename | Derivation::Expr(_) | Derivation::Constant(_) => attribute.clone(),
        Derivation::UnitConversion { factor, offset } => {
            map_value(attribute, |value| match value.as_f64() {
                Some(source) => number_value(source * factor + offset),
                None => value.clone(),
            })
        }
        Derivation::ValueMappings { forward, .. } => {
            map_value(attribute, |value| match forward.get(&plain(value)) {
                Some(target) => target.clone(),
                None => value.clone(),
            })
        }
        Derivation::Cast(cast) => map_value(attribute, |value| cast_value(*cast, value)),
    }
}

/// Applies `transform` to an attribute's value, whatever NGSI-LD shape it is in.
///
/// The attribute types of NGSI-LD's normalized form (CIM 009 clause 4.5).
const ATTRIBUTE_TYPES: [&str; 8] = [
    "Property",
    "GeoProperty",
    "Relationship",
    "LanguageProperty",
    "VocabProperty",
    "JsonProperty",
    "ListProperty",
    "ListRelationship",
];

/// Whether the broker answered in normalized form, judged from the attributes it sent.
///
/// An entity with no attribute to judge by is taken as normalized, the representation a
/// request gets unless it asks for `keyValues`.
fn is_normalized(members: &Map<String, Value>) -> bool {
    let mut attributes = members
        .iter()
        .filter(|(name, _)| {
            let name = name.as_str();
            name != "type"
                && name != "@type"
                && !STRUCTURAL.contains(&name)
                && !SYSTEM.contains(&name)
        })
        .peekable();
    if attributes.peek().is_none() {
        return true;
    }
    attributes.any(|(_, value)| {
        value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| ATTRIBUTE_TYPES.contains(&kind))
    })
}

/// A value a view produces itself (a constant, a computed slot) in the entity's own form.
///
/// A mapping cannot say that a produced value is a geometry or a relationship, so in normalized
/// form it is a `Property`; in key-value form it is the value itself (T-0473).
fn produced(value: Value, normalized: bool) -> Value {
    if normalized {
        serde_json::json!({ "type": "Property", "value": value })
    } else {
        value
    }
}

/// A normalized attribute carries its value under `value` or `object`; a key-value one is the
/// value itself. Both forms reach a view, because the representation is the caller's choice.
fn map_value(attribute: &Value, transform: impl Fn(&Value) -> Value) -> Value {
    let Some(members) = attribute.as_object() else {
        return transform(attribute);
    };
    let Some(key) = ["value", "object"]
        .into_iter()
        .find(|key| members.contains_key(*key))
    else {
        return attribute.clone();
    };
    let mut translated = members.clone();
    translated.insert(key.to_owned(), transform(&members[key]));
    Value::Object(translated)
}

/// One cast, matching the reference engine (DM-39): a value the cast cannot make is left as
/// it is rather than becoming null, because a view must not invent data.
fn cast_value(cast: Cast, value: &Value) -> Value {
    match cast {
        Cast::String => Value::String(plain(value)),
        Cast::Float => match value.as_f64() {
            Some(number) => number_value(number),
            None => plain(value)
                .parse::<f64>()
                .map_or_else(|_| value.clone(), number_value),
        },
        Cast::Integer => match value.as_f64() {
            // Python `int()` truncates towards zero: -4.7 is -4, not -5.
            Some(float) => Value::from(float.trunc() as i64),
            None => plain(value)
                .parse::<i64>()
                .map_or_else(|_| value.clone(), Value::from),
        },
    }
}

/// A JSON number, or a string when the value is not finite: JSON has no NaN.
fn number_value(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or_else(|| Value::Null, Value::Number)
}

/// A JSON value as the plain text a mapping table and a `q` are written with.
fn plain(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// A number as the shortest text that round-trips, so an inverted filter reads like one a
/// person would have written.
fn render(value: f64) -> String {
    let rounded = (value * 1e9).round() / 1e9;
    if rounded.fract() == 0.0 && rounded.abs() < 1e15 {
        return format!("{}", rounded as i64);
    }
    format!("{rounded}")
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

/// The refusal for a target attribute this view does not serve.
fn unknown(name: &str) -> ProblemDetails {
    ProblemDetails::bad_request().with_detail(format!(
        "`{name}` is not an attribute of the model this endpoint serves"
    ))
}

/// The refusal for a slot the mapping cannot invert (DM-51).
fn not_filterable(name: &str) -> ProblemDetails {
    ProblemDetails::bad_request().with_detail(format!(
        "`{name}` is computed by the view and has no source attribute to filter on, so it \
         cannot appear in a query"
    ))
}

/// The refusal for a comparison the slot's derivation cannot invert (T-3672).
fn not_comparable(name: &str, op: CmpOp) -> ProblemDetails {
    ProblemDetails::bad_request().with_detail(format!(
        "`{name}` is translated through a value table, which has no order and no ranges, so \
         `{op}` cannot be asked of it"
    ))
}

/// The refusal every write to a view endpoint answers with (EP-54).
pub fn read_only() -> ProblemDetails {
    ProblemDetails::new(405, "read-only-view", "Method Not Allowed").with_detail(
        "this endpoint serves a mapped view of another model, which is read only: a mapping \
         is invertible per attribute, not per entity, so a write has no source entity to \
         reconstruct",
    )
}

#[cfg(test)]
mod tests {
    use super::{is_normalized, produced};
    use serde_json::{json, Value};

    fn members(entity: Value) -> serde_json::Map<String, Value> {
        entity.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn a_produced_value_takes_the_form_of_the_entity_it_joins() {
        let normalized = members(json!({
            "id": "urn:x", "type": "T", "pm25": { "type": "Property", "value": 1 }
        }));
        let key_values = members(json!({
            "id": "urn:x", "type": "T", "pm25": 1,
            "location": { "type": "Point", "coordinates": [17.0, 48.0] }
        }));
        assert!(is_normalized(&normalized));
        assert!(
            !is_normalized(&key_values),
            "a GeoJSON `type` is not an attribute type"
        );
        assert!(
            is_normalized(&members(json!({ "id": "urn:x", "type": "T" }))),
            "no attribute to judge by: the default representation"
        );
        assert_eq!(
            produced(json!("bb"), true),
            json!({ "type": "Property", "value": "bb" })
        );
        assert_eq!(produced(json!("bb"), false), json!("bb"));
    }
}
