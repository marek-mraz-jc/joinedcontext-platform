//! Edge cases of `pdp::evaluator::conjoin` (T-1920, EP-26, MP-02).
//!
//! Contract, in one sentence: the grants' filters always reach the broker, each in its own
//! parentheses, and the caller's own filter is only ever added as one parenthesized operand — so
//! nothing a caller writes can regroup, weaken or escape a grant's term (ADR 006, R12, R13), and a
//! filter whose parentheses do not balance is dropped instead of being wrapped.
//!
//! `pdp_tests.rs` holds the happy path (a caller's `q` conjoined with one grant). These are the
//! strings written to break out of the parentheses.

use context_gateway::pdp::evaluator::conjoin;

fn filters(of: &[&str]) -> Vec<String> {
    of.iter().map(|filter| (*filter).to_owned()).collect()
}

/// The attack this function exists to stop: a caller filter that closes the wrapper it is about to
/// be put in. It is dropped, and the grant stands alone.
#[test]
fn a_filter_that_closes_its_own_wrapper_is_dropped_and_the_grant_stands() {
    let grants = filters(&["(pm10>=0)"]);
    for hostile in [
        "pm10>0)",
        "pm10>0);(1==1",
        ")",
        "(",
        "((pm10>0)",
        "pm10>0))|(1==1",
        "a==1)|(b==2",
        ")|(",
        "pm10>0;\"unclosed",
        "pm10>0;'unclosed",
    ] {
        let conjoined = conjoin(Some(hostile), &grants);

        assert_eq!(
            conjoined.as_deref(),
            Some("((pm10>=0))"),
            "{hostile:?} was not dropped"
        );
        assert!(
            !conjoined.as_deref().unwrap_or_default().contains("1==1"),
            "{hostile:?} reached the broker"
        );
    }
}

/// A balanced filter is wrapped once, and the grant keeps its own wrapping beside it: the `;` is
/// NGSI-LD's AND, so both terms have to hold.
#[test]
fn a_balanced_filter_is_one_operand_beside_the_grant() {
    let conjoined = conjoin(Some("pm10>50"), &filters(&["(pm10>=0)"]));

    assert_eq!(conjoined.as_deref(), Some("(pm10>50);((pm10>=0))"));
}

/// Parentheses inside a quoted value are data, not grouping: a legitimate filter on a value that
/// contains a bracket still reaches the broker.
#[test]
fn parentheses_inside_a_quoted_value_are_data() {
    for legitimate in [
        r#"name=="Depot (north)""#,
        r#"name=='Depot (north)'"#,
        r#"name=="a\"b""#,
        r#"name=="(((""#,
    ] {
        let conjoined = conjoin(Some(legitimate), &filters(&["(pm10>=0)"]));

        assert_eq!(
            conjoined.as_deref(),
            Some(format!("({legitimate});((pm10>=0))").as_str()),
            "{legitimate:?} was dropped although its brackets are inside a value"
        );
    }
}

/// Several grants are an OR of parenthesized terms, wrapped once: a caller conjoined onto them
/// cannot pair itself with one of them alone.
#[test]
fn several_grants_are_an_or_of_their_own_terms() {
    let conjoined = conjoin(None, &filters(&["(a==1)", "(b==2)", "(c==3)"]));

    assert_eq!(conjoined.as_deref(), Some("(((a==1))|((b==2))|((c==3)))"));
}

/// The caller in front of several grants is still one operand against their whole union.
#[test]
fn a_caller_in_front_of_several_grants_faces_their_union() {
    let conjoined = conjoin(Some("pm10>50"), &filters(&["(a==1)", "(b==2)"]));

    assert_eq!(conjoined.as_deref(), Some("(pm10>50);(((a==1))|((b==2)))"));
}

/// No grant and no caller is no filter at all; a caller alone is its own term, and a dropped
/// caller with no grant leaves nothing rather than an empty expression the broker would reject.
#[test]
fn the_empty_cases_answer_none_and_never_an_empty_expression() {
    assert_eq!(conjoin(None, &[]), None);
    assert_eq!(conjoin(Some(""), &[]).as_deref(), Some("()"));
    assert_eq!(conjoin(Some("pm10>50"), &[]).as_deref(), Some("(pm10>50)"));
    assert_eq!(
        conjoin(Some("pm10>0)"), &[]),
        None,
        "an unbalanced filter with no grant is no filter"
    );
}

/// An empty grant string is still a term: the grants are never dropped, whatever they contain,
/// because dropping one would widen the answer.
#[test]
fn a_grant_is_never_dropped() {
    assert_eq!(conjoin(None, &filters(&[""])).as_deref(), Some("()"));
    // A grant that is itself unbalanced is a broken manifest, not a caller's doing: it is wrapped
    // like any other and reaches the broker, which refuses the whole query. The alternative —
    // dropping it — would answer with an unnarrowed page, so it fails closed this way round.
    assert_eq!(conjoin(None, &filters(&["("])).as_deref(), Some("(()"));
}

/// A control character in a caller's filter cannot split a header or a log line, because the
/// filter is percent-encoded into the query string by the client that forwards it; what this
/// function guarantees is only that it stays inside its own operand.
#[test]
fn a_control_character_stays_inside_the_callers_own_operand() {
    let hostile = "pm10>0\r\nX-Injected: 1";

    let conjoined = conjoin(Some(hostile), &filters(&["(pm10>=0)"])).expect("balanced, so kept");

    assert!(conjoined.starts_with("(pm10>0\r\nX-Injected: 1)"));
    assert!(
        conjoined.ends_with(";((pm10>=0))"),
        "the grant is still there"
    );
}

/// A very long filter is not a way to push the grant out: whatever the caller sends, the grant is
/// the suffix of the expression.
#[test]
fn a_long_filter_cannot_push_the_grant_out() {
    let long = format!("pm10>{}", "0".repeat(4096));

    let conjoined = conjoin(Some(&long), &filters(&["(pm10>=0)"])).expect("balanced, so kept");

    assert!(conjoined.ends_with(";((pm10>=0))"));
}

/// The same inputs always produce the same expression, byte for byte: a verdict has to be
/// reproducible for the dry run to be honest (GW13).
#[test]
fn the_expression_is_byte_for_byte_reproducible() {
    let once = conjoin(Some("pm10>50"), &filters(&["(a==1)", "(b==2)"]));
    for _ in 0..8 {
        assert_eq!(
            conjoin(Some("pm10>50"), &filters(&["(a==1)", "(b==2)"])),
            once
        );
    }
}
