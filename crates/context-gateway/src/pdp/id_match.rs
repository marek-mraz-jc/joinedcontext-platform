//! A grant's id selectors, compiled once per decision instead of once per entity (R24, T-5505).
//!
//! The constraint set carries the anchored patterns as strings, because that is what a test, a
//! log and an equality compare want. The first id a write checks compiles them, and every
//! further entity of the same request (a batch member, a broker page of a held-write probe, the
//! pointing entities of a held delete) reuses the compiled set.

use std::collections::BTreeSet;
use std::ops::Deref;
use std::sync::{Arc, OnceLock};

/// One compiled selector.
#[derive(Debug)]
enum IdMatcher {
    /// An `id` selector, or an `idPattern` that is one escaped literal between `^` and `$`:
    /// matched by string equality, so an id holding `.` or `+` never widens.
    Exact(String),
    /// An `idPattern`.
    Pattern(regex::Regex),
    /// A pattern that does not compile: it matches nothing, so a grant the gateway cannot
    /// evaluate never lets everything through. `jcctl validate` and the Portal reject one
    /// before it is committed; this is the second line.
    Nothing,
}

impl IdMatcher {
    /// The load-time constructor: the only place a pattern becomes a regex.
    fn compile(pattern: &str) -> Self {
        if let Some(literal) = exact(pattern) {
            return Self::Exact(literal);
        }
        regex::Regex::new(pattern).map_or(Self::Nothing, Self::Pattern)
    }

    fn matches(&self, id: &str) -> bool {
        match self {
            Self::Exact(literal) => literal == id,
            Self::Pattern(compiled) => compiled.is_match(id),
            Self::Nothing => false,
        }
    }
}

/// The literal an anchored pattern names when it is nothing but an escaped string, as
/// `evaluator::id_patterns` writes an exact `id`.
fn exact(pattern: &str) -> Option<String> {
    let inner = pattern.strip_prefix('^')?.strip_suffix('$')?;
    let mut literal = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        literal.push(if c == '\\' { chars.next()? } else { c });
    }
    // Escaping the literal again must give the pattern back, so a metacharacter left unescaped
    // (`.`, `*`, a class) or an escape that means something (`\d`) keeps the regex.
    (regex::escape(&literal) == inner).then_some(literal)
}

/// The anchored id patterns of the matching grants; empty is every id.
#[derive(Debug, Clone, Default)]
pub struct IdPatterns {
    patterns: BTreeSet<String>,
    compiled: OnceLock<Arc<[IdMatcher]>>,
}

impl IdPatterns {
    /// Whether `id` falls inside the patterns. No pattern is no restriction.
    pub fn permits(&self, id: &str) -> bool {
        if self.patterns.is_empty() {
            return true;
        }
        self.compiled
            .get_or_init(|| {
                self.patterns
                    .iter()
                    .map(|p| IdMatcher::compile(p))
                    .collect()
            })
            .iter()
            .any(|matcher| matcher.matches(id))
    }
}

impl From<BTreeSet<String>> for IdPatterns {
    fn from(patterns: BTreeSet<String>) -> Self {
        Self {
            patterns,
            compiled: OnceLock::new(),
        }
    }
}

impl FromIterator<String> for IdPatterns {
    fn from_iter<I: IntoIterator<Item = String>>(iter: I) -> Self {
        BTreeSet::from_iter(iter).into()
    }
}

/// Read-only: a changed set is a new value, so the compiled matchers never go stale.
impl Deref for IdPatterns {
    type Target = BTreeSet<String>;

    fn deref(&self) -> &BTreeSet<String> {
        &self.patterns
    }
}

impl PartialEq for IdPatterns {
    fn eq(&self, other: &Self) -> bool {
        self.patterns == other.patterns
    }
}

impl Eq for IdPatterns {}

impl PartialEq<BTreeSet<String>> for IdPatterns {
    fn eq(&self, other: &BTreeSet<String>) -> bool {
        &self.patterns == other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns(list: &[&str]) -> IdPatterns {
        list.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn id_permitted_with_an_uncompilable_pattern_matches_nothing() {
        let broken = patterns(&["^urn:ngsi-ld:Device:(unclosed$"]);
        assert!(!broken.permits("urn:ngsi-ld:Device:(unclosed"));
        assert!(!broken.permits("urn:ngsi-ld:Device:x"));
        assert!(!broken.permits(""));
    }

    #[test]
    fn an_exact_id_selector_matches_only_that_urn_without_a_regex() {
        let urn = "urn:ngsi-ld:Device:bb.sk:lamp+1";
        let anchored = format!("^{}$", regex::escape(urn));
        assert_eq!(exact(&anchored).as_deref(), Some(urn));
        assert!(matches!(IdMatcher::compile(&anchored), IdMatcher::Exact(_)));
        let only = patterns(&[&anchored]);
        assert!(only.permits(urn));
        for other in [
            "urn:ngsi-ld:Device:bbXsk:lamp+1",
            "urn:ngsi-ld:Device:bb.sk:lamppp1",
            "urn:ngsi-ld:Device:bb.sk:lamp+10",
            "xurn:ngsi-ld:Device:bb.sk:lamp+1",
        ] {
            assert!(!only.permits(other), "{other}");
        }
    }

    #[test]
    fn a_pattern_with_metacharacters_stays_a_regex() {
        for pattern in ["^urn:ngsi-ld:Device:.*$", r"^urn:\d+$", "urn:x", "^a$|^b$"] {
            assert_eq!(exact(pattern), None, "{pattern}");
        }
        let prefix = patterns(&["^urn:ngsi-ld:Device:bb\\.sk:.*$"]);
        assert!(prefix.permits("urn:ngsi-ld:Device:bb.sk:lamp-1"));
        assert!(!prefix.permits("urn:ngsi-ld:Device:bbXsk:lamp-1"));
    }

    #[test]
    fn no_pattern_is_no_restriction_and_equality_reads_the_strings() {
        assert!(IdPatterns::default().permits("anything"));
        let a = patterns(&["^x$"]);
        assert!(a.permits("x"));
        assert_eq!(a, patterns(&["^x$"]));
        assert_eq!(a, BTreeSet::from(["^x$".to_owned()]));
    }
}
