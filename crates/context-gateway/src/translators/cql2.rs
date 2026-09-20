//! CQL2-text compiled into the NGSI-LD query the broker already speaks (T-0437, EP-35).
//!
//! A GIS client that sees the CQL2 conformance class offers its user a filter box, so what the
//! class promises has to be what the gateway does. Every predicate of the subset below is
//! rewritten into the `q`, `geoQ` and `temporalQ` parameters the other representations use, and
//! anything outside it is refused with the operator named. A predicate is never dropped: a
//! filter that is ignored returns rows the caller asked not to see, and a client cannot tell the
//! difference between "no matches" and "your filter was thrown away".
//!
//! Three properties are worth stating because they are the reason this is safe.
//!
//! The compiler produces a query and never data: what it emits is intersected with the caller's
//! grants by the PDP afterwards, exactly like a `q` the caller wrote by hand on the NGSI-LD
//! surface (GW10, GW11). A filter can therefore only narrow an answer, never widen one.
//!
//! `NOT` is pushed down to the leaves and turned into the inverse operator, because NGSI-LD has
//! no negation of an expression. Where an operator has no inverse the negation is refused by
//! name rather than approximated — an approximated negation is the one mistake here that
//! returns more than the caller asked for.
//!
//! NGSI-LD carries one `geoQ` and one `temporalQ` per query, so a spatial or temporal predicate
//! is only expressible on the top-level `AND` spine. One under an `OR`, or a second of the same
//! kind, is a `400` naming it rather than a filter that is quietly half applied.

use crate::translators::ogc::ParamError;
use serde_json::{json, Value};

/// The parameter every refusal from this module names.
const FILTER: &str = "filter";

/// The filter languages this endpoint accepts, which is the one CQL2 has a text form of.
pub const LANG: &str = "cql2-text";

/// What a filter becomes: the NGSI-LD parameters the handler puts on the upstream query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Compiled {
    /// The scalar predicates as an NGSI-LD `q`, absent when the filter was purely spatial.
    pub q: Option<String>,
    /// `georel`, `geometry` and `coordinates`, in that order, or empty.
    pub geo: Vec<(String, String)>,
    /// `timerel`, `timeAt` and possibly `endTimeAt`, or empty.
    pub temporal: Vec<(String, String)>,
}

/// Compiles one CQL2-text filter (EP-35).
///
/// The caller has already checked `filter-lang`; an empty filter is a caller mistake rather
/// than a no-op, because a client that sends one believes it narrowed something.
pub fn compile(text: &str) -> Result<Compiled, ParamError> {
    let tokens = scan(text)?;
    let mut parser = Parser {
        tokens,
        at: 0,
        depth: 0,
    };
    let expr = parser.expression()?;
    parser.end()?;
    let expr = push_not(expr, false)?;

    let mut compiled = Compiled::default();
    let mut scalars = Vec::new();
    for conjunct in spine(expr) {
        match conjunct {
            Expr::Spatial(spatial) => {
                if !compiled.geo.is_empty() {
                    return Err(refuse(
                        "only one spatial predicate can be applied to a query",
                    ));
                }
                compiled.geo = spatial.into_params();
            }
            Expr::Temporal(temporal) => {
                if !compiled.temporal.is_empty() {
                    return Err(refuse(
                        "only one temporal predicate can be applied to a query",
                    ));
                }
                compiled.temporal = temporal.into_params();
            }
            other => scalars.push(render(&other)?),
        }
    }
    if !scalars.is_empty() {
        compiled.q = Some(scalars.join(";"));
    }
    Ok(compiled)
}

fn refuse(detail: impl Into<String>) -> ParamError {
    ParamError {
        parameter: FILTER,
        detail: detail.into(),
    }
}

/// The operator a client named that this endpoint does not implement.
fn unsupported(operator: &str) -> ParamError {
    refuse(format!(
        "CQL2 operator {} is not supported by this endpoint",
        operator.to_ascii_uppercase()
    ))
}

// ---------------------------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// A bare run: an identifier, a keyword or a number literal, as written.
    Word(String),
    /// A single-quoted string, unescaped (`''` is one quote).
    Text(String),
    /// A comparison operator, normalised to one of `= <> < <= > >=`.
    Op(&'static str),
    Open,
    Close,
    Comma,
}

/// Splits the filter into tokens, refusing a character the grammar has no place for.
///
/// A lexical error is a `400` here rather than a strange parse three functions later, and it
/// names the character, because a client that only hears "invalid filter" sends it again.
fn scan(text: &str) -> Result<Vec<Token>, ParamError> {
    let bytes: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let c = bytes[at];
        if c.is_whitespace() {
            at += 1;
            continue;
        }
        match c {
            '(' => {
                tokens.push(Token::Open);
                at += 1;
            }
            ')' => {
                tokens.push(Token::Close);
                at += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                at += 1;
            }
            '\'' => {
                let (text, next) = quoted(&bytes, at)?;
                // The value is written into the NGSI-LD `q` between double quotes, so a value
                // carrying one would close that string early and the rest of it would become
                // structure. The `q` is then unbalanced, the evaluator drops the caller's filter
                // whole (`conjoin`, `is_balanced`) and the answer carries the rows the filter was
                // meant to remove. A `400` naming the value is the honest answer (T-2348).
                if text.contains('"') {
                    return Err(refuse("a value may not contain a double quote"));
                }
                tokens.push(Token::Text(text));
                at = next;
            }
            '"' => {
                let (name, next) = double_quoted(&bytes, at)?;
                tokens.push(Token::Word(identifier(name)?));
                at = next;
            }
            '<' | '>' | '=' | '!' => {
                let (op, next) = operator(&bytes, at)?;
                tokens.push(Token::Op(op));
                at = next;
            }
            _ => {
                let (word, next) = word(&bytes, at)?;
                tokens.push(Token::Word(word));
                at = next;
            }
        }
    }
    if tokens.is_empty() {
        return Err(refuse("the filter is empty"));
    }
    Ok(tokens)
}

/// A `'...'` literal, where a quote inside it is written twice.
fn quoted(chars: &[char], at: usize) -> Result<(String, usize), ParamError> {
    let mut out = String::new();
    let mut i = at + 1;
    while i < chars.len() {
        if chars[i] == '\'' {
            if chars.get(i + 1) == Some(&'\'') {
                out.push('\'');
                i += 2;
                continue;
            }
            return Ok((out, i + 1));
        }
        out.push(chars[i]);
        i += 1;
    }
    Err(refuse("a quoted value is never closed"))
}

/// The characters an attribute name may carry into the NGSI-LD `q` (T-2348).
///
/// A double-quoted identifier is CQL2's escape hatch for a name that is not a bare word, and what
/// it carries is written into the `q` unquoted: `"a)|(b" > 5` became `a)|(b>5`, which is not a
/// balanced filter, so `conjoin` dropped the caller's whole filter and the caller got the rows it
/// asked not to see. The set below is exactly what `word` accepts for a bare name, which is what
/// an NGSI-LD attribute name may be made of anyway (CIM 009 4.9).
fn identifier(name: String) -> Result<String, ParamError> {
    let usable = |c: char| c.is_alphanumeric() || matches!(c, '_' | '.' | ':' | '-');
    if name.is_empty() || !name.chars().all(usable) {
        return Err(refuse(format!(
            "{name} is not an attribute name: a name is made of letters, digits, `_`, `.`, `:` and `-`"
        )));
    }
    Ok(name)
}

/// A `"..."` identifier, which CQL2 allows for a name that is not a bare word.
fn double_quoted(chars: &[char], at: usize) -> Result<(String, usize), ParamError> {
    let mut out = String::new();
    let mut i = at + 1;
    while i < chars.len() {
        if chars[i] == '"' {
            return Ok((out, i + 1));
        }
        out.push(chars[i]);
        i += 1;
    }
    Err(refuse("a quoted identifier is never closed"))
}

/// One comparison operator, in any of the spellings CQL2 and its clients use.
fn operator(chars: &[char], at: usize) -> Result<(&'static str, usize), ParamError> {
    let two: String = chars[at..(at + 2).min(chars.len())].iter().collect();
    let op = match two.as_str() {
        "<>" | "!=" => return Ok(("<>", at + 2)),
        "<=" => return Ok(("<=", at + 2)),
        ">=" => return Ok((">=", at + 2)),
        _ => match chars[at] {
            '=' => "=",
            '<' => "<",
            '>' => ">",
            other => return Err(refuse(format!("{other} is not an operator"))),
        },
    };
    Ok((op, at + 1))
}

/// A bare word: an identifier, a keyword, or a number with its sign and decimals.
fn word(chars: &[char], at: usize) -> Result<(String, usize), ParamError> {
    let mut i = at;
    if chars[i] == '-' || chars[i] == '+' {
        i += 1;
    }
    let start = i;
    while i < chars.len() {
        let c = chars[i];
        if c.is_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
            i += 1;
            continue;
        }
        break;
    }
    if i == start {
        return Err(refuse(format!("{} is not a name or a value", chars[at])));
    }
    Ok((chars[at..i].iter().collect(), i))
}

// ---------------------------------------------------------------------------------------------
// Syntax
// ---------------------------------------------------------------------------------------------

/// A filter, after parsing and before `NOT` is pushed to the leaves.
#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Or(Vec<Expr>),
    And(Vec<Expr>),
    Not(Box<Expr>),
    /// `name op value`, `name LIKE 'x%'`, `name BETWEEN a AND b`, `name IN (…)`, `name IS NULL`.
    Scalar(Scalar),
    Spatial(Spatial),
    Temporal(Temporal),
}

#[derive(Debug, Clone, PartialEq)]
struct Scalar {
    name: String,
    /// The NGSI-LD operator this predicate carries, already inverted if it was negated.
    kind: ScalarKind,
}

#[derive(Debug, Clone, PartialEq)]
enum ScalarKind {
    /// A comparison against one rendered NGSI-LD value.
    Compare(&'static str, String),
    /// `BETWEEN`, as the NGSI-LD range `low..high`, negated or not.
    Range {
        low: String,
        high: String,
        negated: bool,
    },
    /// `IN`, as the NGSI-LD value list, negated or not.
    List { values: Vec<String>, negated: bool },
    /// `IS NULL`: the attribute must be absent, or present when negated.
    Missing { negated: bool },
    /// `LIKE`, as the NGSI-LD pattern match. It has no negation NGSI-LD can express.
    Pattern(String),
}

#[derive(Debug, Clone, PartialEq)]
struct Spatial {
    georel: &'static str,
    geometry: String,
    coordinates: String,
    /// The GeoProperty the filter is applied to, which NGSI-LD calls the geo property.
    geoproperty: String,
}

impl Spatial {
    fn into_params(self) -> Vec<(String, String)> {
        let mut params = vec![
            ("georel".to_owned(), self.georel.to_owned()),
            ("geometry".to_owned(), self.geometry),
            ("coordinates".to_owned(), self.coordinates),
        ];
        // `location` is the NGSI-LD default, so naming it changes nothing and naming anything
        // else is the whole point of carrying it.
        if self.geoproperty != "location" {
            params.push(("geoproperty".to_owned(), self.geoproperty));
        }
        params
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Temporal {
    timerel: &'static str,
    time_at: String,
    end_time_at: Option<String>,
    /// The attribute the interval is measured on, which NGSI-LD calls the time property.
    timeproperty: String,
}

impl Temporal {
    fn into_params(self) -> Vec<(String, String)> {
        let mut params = vec![
            ("timerel".to_owned(), self.timerel.to_owned()),
            ("timeAt".to_owned(), self.time_at),
        ];
        if let Some(end) = self.end_time_at {
            params.push(("endTimeAt".to_owned(), end));
        }
        // `observedAt` is the NGSI-LD default, so naming it changes nothing and naming
        // anything else is the whole point of carrying it.
        if self.timeproperty != "observedAt" {
            params.push(("timeproperty".to_owned(), self.timeproperty));
        }
        params
    }
}

/// How deep a filter may nest. Every parenthesis and every `NOT` is one frame of the parser and
/// one level of the tree that `push_not`, `spine`, `render` and `Drop` walk recursively
/// afterwards, so one number bounds all of them. A filter a person writes is two or three deep;
/// thirty-two is far past anything a client generates and far short of a stack.
const MAX_DEPTH: usize = 32;

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    /// Open parentheses and `NOT`s the parser is currently inside (EP-35, R5).
    depth: usize,
}

impl Parser {
    /// One level deeper, or the refusal a filter gets for nesting past the ceiling.
    ///
    /// Without it the parser recurses once per opening parenthesis, and a query string of a few
    /// thousand `(` overflows the request thread's stack — which aborts the whole process, not
    /// the request, because a stack overflow is not a panic anything can catch (R5).
    fn deeper(&mut self) -> Result<(), ParamError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(refuse(format!(
                "the filter nests more than {MAX_DEPTH} levels deep"
            )));
        }
        Ok(())
    }

    /// Back out of one level. Only the success path unwinds: a refusal ends the whole parse.
    fn shallower(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn keyword(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(found)) if found.eq_ignore_ascii_case(word))
    }

    fn take_keyword(&mut self, word: &str) -> bool {
        let found = self.keyword(word);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect(&mut self, token: &Token, what: &str) -> Result<(), ParamError> {
        if self.peek() == Some(token) {
            self.at += 1;
            return Ok(());
        }
        Err(refuse(format!("expected {what}")))
    }

    fn end(&self) -> Result<(), ParamError> {
        match self.peek() {
            None => Ok(()),
            Some(_) => Err(refuse("the filter has more text after its last predicate")),
        }
    }

    fn expression(&mut self) -> Result<Expr, ParamError> {
        let mut branches = vec![self.conjunction()?];
        while self.take_keyword("or") {
            branches.push(self.conjunction()?);
        }
        Ok(if branches.len() == 1 {
            branches.remove(0)
        } else {
            Expr::Or(branches)
        })
    }

    fn conjunction(&mut self) -> Result<Expr, ParamError> {
        let mut branches = vec![self.negation()?];
        while self.take_keyword("and") {
            branches.push(self.negation()?);
        }
        Ok(if branches.len() == 1 {
            branches.remove(0)
        } else {
            Expr::And(branches)
        })
    }

    fn negation(&mut self) -> Result<Expr, ParamError> {
        if self.take_keyword("not") {
            self.deeper()?;
            let inner = self.negation()?;
            self.shallower();
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.predicate()
    }

    fn predicate(&mut self) -> Result<Expr, ParamError> {
        if self.peek() == Some(&Token::Open) {
            self.at += 1;
            self.deeper()?;
            let inner = self.expression()?;
            self.expect(&Token::Close, "a closing parenthesis")?;
            self.shallower();
            return Ok(inner);
        }
        let Some(Token::Word(word)) = self.peek().cloned() else {
            return Err(refuse("expected a property name or a function"));
        };
        // A word directly followed by `(` is a function call, which in this subset is a
        // spatial or temporal predicate and nothing else.
        if self.tokens.get(self.at + 1) == Some(&Token::Open) {
            return self.call(&word);
        }
        self.at += 1;
        self.comparison(word)
    }

    /// `name <op> value`, `name LIKE`, `name BETWEEN`, `name IN`, `name IS NULL`.
    fn comparison(&mut self, name: String) -> Result<Expr, ParamError> {
        if self.take_keyword("like") {
            let pattern = self.literal()?;
            return Ok(Expr::Scalar(Scalar {
                name,
                kind: ScalarKind::Pattern(pattern_of(&pattern)),
            }));
        }
        if self.take_keyword("between") {
            let low = self.literal()?;
            if !self.take_keyword("and") {
                return Err(refuse("BETWEEN needs a low and a high value joined by AND"));
            }
            let high = self.literal()?;
            return Ok(Expr::Scalar(Scalar {
                name,
                kind: ScalarKind::Range {
                    low: value_of(&low),
                    high: value_of(&high),
                    negated: false,
                },
            }));
        }
        if self.take_keyword("in") {
            self.expect(&Token::Open, "a list in parentheses after IN")?;
            let mut values = vec![value_of(&self.literal()?)];
            while self.peek() == Some(&Token::Comma) {
                self.at += 1;
                values.push(value_of(&self.literal()?));
            }
            self.expect(&Token::Close, "a closing parenthesis after the IN list")?;
            return Ok(Expr::Scalar(Scalar {
                name,
                kind: ScalarKind::List {
                    values,
                    negated: false,
                },
            }));
        }
        if self.take_keyword("is") {
            let negated = self.take_keyword("not");
            if !self.take_keyword("null") {
                return Err(unsupported("IS"));
            }
            return Ok(Expr::Scalar(Scalar {
                name,
                kind: ScalarKind::Missing { negated },
            }));
        }
        let Some(Token::Op(op)) = self.peek().cloned() else {
            return Err(refuse(format!("{name} is followed by no operator")));
        };
        self.at += 1;
        let value = self.literal()?;
        let rendered = match op {
            "=" => "==",
            "<>" => "!=",
            other => other,
        };
        Ok(Expr::Scalar(Scalar {
            name,
            kind: ScalarKind::Compare(rendered, value_of(&value)),
        }))
    }

    /// A function call: the spatial and temporal predicates, and nothing else.
    fn call(&mut self, name: &str) -> Result<Expr, ParamError> {
        let upper = name.to_ascii_uppercase();
        self.at += 1;
        self.expect(&Token::Open, "an argument list")?;
        let arguments = self.arguments()?;
        match upper.as_str() {
            "S_INTERSECTS" | "S_WITHIN" => {
                let [property, geometry] = two(&arguments, &upper)?;
                let (kind, coordinates) = wkt(geometry)?;
                Ok(Expr::Spatial(Spatial {
                    georel: if upper == "S_WITHIN" {
                        "within"
                    } else {
                        "intersects"
                    },
                    geometry: kind,
                    coordinates,
                    geoproperty: property.to_owned(),
                }))
            }
            "T_AFTER" | "T_BEFORE" => {
                let [property, instant] = two(&arguments, &upper)?;
                Ok(Expr::Temporal(Temporal {
                    timerel: if upper == "T_AFTER" {
                        "after"
                    } else {
                        "before"
                    },
                    time_at: instant_of(instant)?,
                    end_time_at: None,
                    timeproperty: property.to_owned(),
                }))
            }
            "T_DURING" => {
                let [property, interval] = two(&arguments, &upper)?;
                let (start, end) = interval_of(interval)?;
                Ok(Expr::Temporal(Temporal {
                    timerel: "between",
                    time_at: start,
                    end_time_at: Some(end),
                    timeproperty: property.to_owned(),
                }))
            }
            // The literal constructors are arguments, never predicates of their own.
            "TIMESTAMP" | "DATE" | "INTERVAL" | "POINT" | "POLYGON" | "LINESTRING" | "BBOX" => {
                Err(refuse(format!("{upper} is a value, not a predicate")))
            }
            _ => Err(unsupported(&upper)),
        }
    }

    /// The arguments of a call, each kept as its source text so a nested literal survives.
    fn arguments(&mut self) -> Result<Vec<String>, ParamError> {
        let mut arguments = Vec::new();
        let mut current = String::new();
        let mut depth = 0usize;
        loop {
            match self.peek().cloned() {
                None => return Err(refuse("an argument list is never closed")),
                Some(Token::Close) if depth == 0 => {
                    self.at += 1;
                    arguments.push(current.trim().to_owned());
                    return Ok(arguments);
                }
                Some(Token::Comma) if depth == 0 => {
                    self.at += 1;
                    arguments.push(current.trim().to_owned());
                    current = String::new();
                }
                Some(token) => {
                    self.at += 1;
                    match token {
                        Token::Open => {
                            depth += 1;
                            current.push('(');
                        }
                        Token::Close => {
                            depth -= 1;
                            current.push(')');
                        }
                        Token::Comma => current.push(','),
                        Token::Word(word) => {
                            if !current.is_empty() && !current.ends_with(['(', ',']) {
                                current.push(' ');
                            }
                            current.push_str(&word);
                        }
                        Token::Text(text) => {
                            current.push('\'');
                            current.push_str(&text);
                            current.push('\'');
                        }
                        Token::Op(op) => current.push_str(op),
                    }
                }
            }
        }
    }

    /// One value: a quoted string, a number, or a bare word used as a value.
    fn literal(&mut self) -> Result<String, ParamError> {
        match self.peek().cloned() {
            Some(Token::Text(text)) => {
                self.at += 1;
                Ok(format!("'{text}'"))
            }
            Some(Token::Word(word)) => {
                self.at += 1;
                Ok(word)
            }
            _ => Err(refuse("expected a value")),
        }
    }
}

/// Exactly two arguments, because every predicate of this subset is binary.
fn two<'a>(arguments: &'a [String], operator: &str) -> Result<[&'a str; 2], ParamError> {
    match arguments {
        [first, second] => Ok([first.as_str(), second.as_str()]),
        _ => Err(refuse(format!(
            "{operator} takes two arguments, a property and a value"
        ))),
    }
}

// ---------------------------------------------------------------------------------------------
// Negation and rendering
// ---------------------------------------------------------------------------------------------

/// Pushes every `NOT` down to a leaf and turns it into the inverse operator.
///
/// NGSI-LD has no negation of an expression, so this is the only way `NOT` can be honoured at
/// all. De Morgan handles the connectives; a leaf whose operator has no inverse is refused by
/// name here rather than widened into something that returns more rows.
fn push_not(expr: Expr, negated: bool) -> Result<Expr, ParamError> {
    Ok(match (expr, negated) {
        (Expr::Not(inner), _) => push_not(*inner, !negated)?,
        (Expr::And(branches), false) | (Expr::Or(branches), true) => Expr::And(
            branches
                .into_iter()
                .map(|branch| push_not(branch, negated))
                .collect::<Result<_, _>>()?,
        ),
        (Expr::Or(branches), false) | (Expr::And(branches), true) => Expr::Or(
            branches
                .into_iter()
                .map(|branch| push_not(branch, negated))
                .collect::<Result<_, _>>()?,
        ),
        (expr, false) => expr,
        (Expr::Scalar(scalar), true) => Expr::Scalar(Scalar {
            name: scalar.name,
            kind: match scalar.kind {
                ScalarKind::Compare(op, value) => ScalarKind::Compare(inverse(op)?, value),
                ScalarKind::Range { low, high, negated } => ScalarKind::Range {
                    low,
                    high,
                    negated: !negated,
                },
                ScalarKind::List { values, negated } => ScalarKind::List {
                    values,
                    negated: !negated,
                },
                ScalarKind::Missing { negated } => ScalarKind::Missing { negated: !negated },
                // `~=` is a regular expression match and NGSI-LD has no `!~`.
                ScalarKind::Pattern(_) => return Err(unsupported("NOT LIKE")),
            },
        }),
        (Expr::Spatial(spatial), true) => Expr::Spatial(Spatial {
            // `disjoint` is the NGSI-LD inverse of `intersects`; `within` has none, because
            // "not inside" is not "outside" for a geometry that straddles the boundary.
            georel: match spatial.georel {
                "intersects" => "disjoint",
                _ => return Err(unsupported("NOT S_WITHIN")),
            },
            ..spatial
        }),
        (Expr::Temporal(temporal), true) => Expr::Temporal(Temporal {
            timerel: match temporal.timerel {
                "after" => "before",
                "before" => "after",
                _ => return Err(unsupported("NOT T_DURING")),
            },
            ..temporal
        }),
    })
}

/// The NGSI-LD operator that means the opposite of this one.
fn inverse(op: &str) -> Result<&'static str, ParamError> {
    Ok(match op {
        "==" => "!=",
        "!=" => "==",
        ">" => "<=",
        ">=" => "<",
        "<" => ">=",
        "<=" => ">",
        other => return Err(unsupported(other)),
    })
}

/// The conjuncts of a filter: what an NGSI-LD query can carry beside a `geoQ`.
fn spine(expr: Expr) -> Vec<Expr> {
    match expr {
        Expr::And(branches) => branches.into_iter().flat_map(spine).collect(),
        other => vec![other],
    }
}

/// One expression as an NGSI-LD `q` fragment.
///
/// A spatial or temporal predicate that reaches here sits under an `OR`, where NGSI-LD cannot
/// put it: `geoQ` applies to the whole query, so folding it into one branch of a disjunction
/// would apply it to the other branch too.
fn render(expr: &Expr) -> Result<String, ParamError> {
    Ok(match expr {
        Expr::Scalar(scalar) => scalar_q(scalar),
        Expr::Or(branches) => format!(
            "({})",
            branches
                .iter()
                .map(render)
                .collect::<Result<Vec<_>, _>>()?
                .join("|")
        ),
        Expr::And(branches) => format!(
            "({})",
            branches
                .iter()
                .map(render)
                .collect::<Result<Vec<_>, _>>()?
                .join(";")
        ),
        Expr::Spatial(_) => {
            return Err(refuse(
                "a spatial predicate applies to the whole query, so it cannot sit inside an OR",
            ))
        }
        Expr::Temporal(_) => {
            return Err(refuse(
                "a temporal predicate applies to the whole query, so it cannot sit inside an OR",
            ))
        }
        // `push_not` removed every `Not` before this walk.
        Expr::Not(_) => return Err(refuse("the filter could not be normalised")),
    })
}

fn scalar_q(scalar: &Scalar) -> String {
    let name = &scalar.name;
    match &scalar.kind {
        ScalarKind::Compare(op, value) => format!("{name}{op}{value}"),
        ScalarKind::Range {
            low,
            high,
            negated: false,
        } => format!("{name}=={low}..{high}"),
        ScalarKind::Range {
            low,
            high,
            negated: true,
        } => format!("{name}!={low}..{high}"),
        ScalarKind::List {
            values,
            negated: false,
        } => format!("{name}=={}", values.join(",")),
        ScalarKind::List {
            values,
            negated: true,
        } => format!("{name}!={}", values.join(",")),
        // NGSI-LD writes "the attribute is absent" as `!name` and "it is there" as the bare
        // name, which is exactly what IS NULL and IS NOT NULL mean about a Property.
        ScalarKind::Missing { negated: false } => format!("!{name}"),
        ScalarKind::Missing { negated: true } => name.clone(),
        ScalarKind::Pattern(pattern) => format!("{name}~=\"{pattern}\""),
    }
}

// ---------------------------------------------------------------------------------------------
// Literals
// ---------------------------------------------------------------------------------------------

/// One CQL2 literal as NGSI-LD writes it: a string keeps quotes, a number does not.
fn value_of(literal: &str) -> String {
    match literal
        .strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
    {
        Some(text) => format!("\"{text}\""),
        None => literal.to_owned(),
    }
}

/// A `LIKE` pattern as the regular expression NGSI-LD's `~=` takes.
///
/// `%` and `_` are the two SQL wildcards CQL2 inherits; every other character is matched
/// literally, so a pattern carrying `.` or `*` cannot turn into a wider regular expression than
/// the caller wrote. Anchored, because `LIKE` matches the whole value and `~=` does not.
fn pattern_of(literal: &str) -> String {
    let text = literal
        .strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
        .unwrap_or(literal);
    let mut pattern = String::from("^");
    for c in text.chars() {
        match c {
            '%' => pattern.push_str(".*"),
            '_' => pattern.push('.'),
            c if c.is_alphanumeric() || c == ' ' => pattern.push(c),
            c => {
                pattern.push('\\');
                pattern.push(c);
            }
        }
    }
    pattern.push('$');
    pattern
}

/// `TIMESTAMP('…')`, `DATE('…')` or a bare quoted instant, as RFC 3339.
fn instant_of(argument: &str) -> Result<String, ParamError> {
    let inner = strip_call(argument, &["TIMESTAMP", "DATE"]).unwrap_or(argument);
    let text = inner.trim().trim_matches('\'');
    // A date is a day, and the instant NGSI-LD compares against is its start.
    let candidate = if text.len() == 10 {
        format!("{text}T00:00:00Z")
    } else {
        text.to_owned()
    };
    chrono::DateTime::parse_from_rfc3339(&candidate)
        .map(|_| candidate)
        .map_err(|_| refuse(format!("{text} is not an RFC 3339 instant")))
}

/// `INTERVAL('start','end')` as the two instants of an NGSI-LD `between`.
fn interval_of(argument: &str) -> Result<(String, String), ParamError> {
    let Some(inner) = strip_call(argument, &["INTERVAL"]) else {
        return Err(refuse("T_DURING takes an INTERVAL('start','end')"));
    };
    let Some((start, end)) = inner.split_once(',') else {
        return Err(refuse("an INTERVAL has a start and an end"));
    };
    let (start, end) = (instant_of(start)?, instant_of(end)?);
    if start > end {
        return Err(refuse("the interval ends before it starts"));
    }
    Ok((start, end))
}

/// The inside of `NAME( … )` when the text is a call to one of these names.
fn strip_call<'a>(text: &'a str, names: &[&str]) -> Option<&'a str> {
    let trimmed = text.trim();
    let open = trimmed.find('(')?;
    let name = trimmed[..open].trim();
    if !names.iter().any(|wanted| name.eq_ignore_ascii_case(wanted)) {
        return None;
    }
    trimmed[open + 1..].strip_suffix(')')
}

/// A WKT geometry literal as the NGSI-LD `geometry` and `coordinates` pair.
///
/// The four shapes a filter box actually produces. A geometry collection is refused rather than
/// flattened, because NGSI-LD's `geoQ` takes one geometry and flattening one would change the
/// area the caller asked about.
fn wkt(text: &str) -> Result<(String, String), ParamError> {
    let trimmed = text.trim();
    let Some(open) = trimmed.find('(') else {
        return Err(refuse(format!("{trimmed} is not a geometry literal")));
    };
    let name = trimmed[..open].trim().to_ascii_uppercase();
    let Some(body) = trimmed[open..]
        .strip_prefix('(')
        .and_then(|b| b.strip_suffix(')'))
    else {
        return Err(refuse("a geometry literal is never closed"));
    };
    let coordinates = match name.as_str() {
        "POINT" => position(body)?,
        "LINESTRING" => Value::Array(positions(body)?),
        "POLYGON" => Value::Array(rings(body)?),
        // `BBOX(minx,miny,maxx,maxy)` is the envelope CQL2 spells as a literal; NGSI-LD has
        // no envelope, so it becomes the polygon it describes.
        "BBOX" => Value::Array(vec![Value::Array(envelope(body)?)]),
        other => return Err(unsupported(other)),
    };
    let kind = match name.as_str() {
        "BBOX" => "Polygon".to_owned(),
        "POINT" => "Point".to_owned(),
        "LINESTRING" => "LineString".to_owned(),
        _ => "Polygon".to_owned(),
    };
    Ok((kind, coordinates.to_string()))
}

/// `x y` as a GeoJSON position.
fn position(text: &str) -> Result<Value, ParamError> {
    let numbers: Vec<f64> = text
        .split_whitespace()
        .map(|part| part.parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| refuse(format!("{text} is not a coordinate pair")))?;
    match numbers.as_slice() {
        // A third ordinate is an elevation NGSI-LD's 2D geoQ cannot carry; dropping it widens
        // nothing, because the horizontal footprint is the same.
        [x, y] | [x, y, _] => Ok(json!([x, y])),
        _ => Err(refuse(format!("{text} is not a coordinate pair"))),
    }
}

/// `x y, x y, …` as a list of positions.
fn positions(text: &str) -> Result<Vec<Value>, ParamError> {
    text.split(',').map(position).collect()
}

/// `(…), (…)` as the rings of a polygon, or one bare ring.
fn rings(text: &str) -> Result<Vec<Value>, ParamError> {
    let trimmed = text.trim();
    if !trimmed.starts_with('(') {
        return Ok(vec![Value::Array(positions(trimmed)?)]);
    }
    let mut rings = Vec::new();
    let mut rest = trimmed;
    while let Some(open) = rest.find('(') {
        let Some(close) = rest[open..].find(')') else {
            return Err(refuse("a polygon ring is never closed"));
        };
        rings.push(Value::Array(positions(&rest[open + 1..open + close])?));
        rest = &rest[open + close + 1..];
    }
    if rings.is_empty() {
        return Err(refuse("a polygon needs at least one ring"));
    }
    Ok(rings)
}

/// `minx,miny,maxx,maxy` as the closed ring of that box.
fn envelope(text: &str) -> Result<Vec<Value>, ParamError> {
    let numbers: Vec<f64> = text
        .split(',')
        .map(|part| part.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| refuse("a BBOX takes four numbers"))?;
    let [minx, miny, maxx, maxy] = numbers[..] else {
        return Err(refuse("a BBOX takes four numbers"));
    };
    if minx > maxx || miny > maxy {
        return Err(refuse(
            "the lower corner of the BBOX must not be greater than the upper corner",
        ));
    }
    Ok(vec![
        json!([minx, miny]),
        json!([maxx, miny]),
        json!([maxx, maxy]),
        json!([minx, maxy]),
        json!([minx, miny]),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_nested_past_the_ceiling_is_refused_and_the_process_survives() {
        // Without the depth guard this recurses once per parenthesis and overflows the request
        // thread's stack, which aborts the process rather than the request: a stack overflow is
        // not a panic anything catches (R5). The refusal is the whole point of the test — if it
        // ever regresses, this test takes the test binary down with it, loudly.
        let deep = format!("{}pm10>50{}", "(".repeat(10_000), ")".repeat(10_000));
        let refused = compile(&deep).expect_err("a filter that deep is refused");
        assert_eq!(refused.parameter, FILTER);
        assert!(refused.detail.contains("nests more than"), "{refused:?}");
    }

    #[test]
    fn a_chain_of_negations_is_bounded_the_same_way() {
        // `NOT` recurses through `negation` rather than `predicate`, so it needs the same
        // counter; a thousand of them is the same overflow by another door.
        let deep = format!("{}pm10>50", "NOT ".repeat(1_000));
        assert!(compile(&deep).is_err(), "a thousand NOTs were accepted");
    }

    #[test]
    fn a_filter_at_the_ceiling_still_compiles() {
        // The ceiling has to be a ceiling and not a trap: anything a client actually generates
        // is two or three deep, and the depth that is allowed must work.
        let deep = format!("{}pm10>50{}", "(".repeat(MAX_DEPTH), ")".repeat(MAX_DEPTH));
        compile(&deep).expect("a filter at the ceiling compiles");

        let over = format!(
            "{}pm10>50{}",
            "(".repeat(MAX_DEPTH + 1),
            ")".repeat(MAX_DEPTH + 1)
        );
        assert!(compile(&over).is_err(), "one level past the ceiling passed");
    }

    #[test]
    fn nesting_is_counted_per_branch_and_not_across_the_whole_filter() {
        // A wide filter is not a deep one: ten predicates side by side each go one level down
        // and come back up, and refusing that would refuse ordinary queries.
        let wide = (0..64)
            .map(|n| format!("(pm10>{n})"))
            .collect::<Vec<_>>()
            .join(" OR ");
        compile(&wide).expect("a wide filter is not a deep one");
    }
}
