//! Hierarchical scopes as an anchored regex on the entity's own `scope` (T-0148, R13, R30).
//!
//! A grant on `/geo/SK/BB` covers the district and everything under it, and nothing
//! beside it. Sent as a separate `scopeQ` parameter that would be true, and unusable:
//! ADR 006 is about what happens when two policies apply and their parameters are merged
//! per parameter — the caller ends up with one policy's `q` and the other's scope, which
//! neither policy granted. R12 and R13 answer that by folding each policy's scopes into
//! that policy's own `q` term, so a policy is one conjunction and the policies are OR-ed
//! whole.
//!
//! The regex is anchored at both ends and the separator is explicit: `/geo/SK/BB` matches
//! `/geo/SK/BB` and `/geo/SK/BB/Sasova`, never `/geo/SK/BBB`, which is the whole point
//! (R30).

/// The `q` term one `scopeQ` becomes, or `None` when it names no scope path.
///
/// The scope query language separates paths with `;`, `|` and parentheses; the union is
/// what a grant covers, so the folded term is an OR of one regex per path. A caller that
/// holds a grant on two districts may read either, and no expression mixes them.
pub fn fold(scope_q: &str) -> Option<String> {
    let terms: Vec<String> = paths(scope_q).map(|path| term(&path)).collect();
    match terms.len() {
        0 => None,
        1 => Some(terms.into_iter().next().expect("one term")),
        _ => Some(format!("({})", terms.join("|"))),
    }
}

/// One anchored regex term for one scope path.
fn term(path: &str) -> String {
    // The path is a literal: a dot or a `+` in a scope name is a character, not a regex
    // operator, and a scope somebody names `a.b` must not match `axb`.
    let literal = regex::escape(path);
    format!("scope~=\"^{literal}(/.*)?$\"")
}

/// The individual scope paths of a scope query, whose operators separate the paths.
///
/// A trailing `/#` is the subtree wildcard of the scope query language and a trailing `/`
/// is a typo; both mean the same subtree the anchored regex already covers, so they are
/// trimmed rather than escaped into the pattern.
fn paths(scope_q: &str) -> impl Iterator<Item = String> + '_ {
    scope_q
        .split(['(', ')', ';', '|', ','])
        .map(str::trim)
        .filter(|path| path.starts_with('/'))
        .map(|path| path.trim_end_matches("/#").trim_end_matches('/').to_owned())
        .filter(|path| !path.is_empty())
}
