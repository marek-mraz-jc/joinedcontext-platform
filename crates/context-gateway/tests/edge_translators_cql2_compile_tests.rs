//! Edge cases of `translators::cql2::compile` (T-1948, T-2348, EP-35, EP-26, MP-02).
//!
//! Contract, in one sentence: a filter in the CQL2 subset this endpoint claims becomes the
//! NGSI-LD `q`, `geoQ` and `temporalQ` the broker already speaks, and everything else — an
//! operator outside the subset, a negation NGSI-LD cannot express, a second spatial or temporal
//! predicate, a name or value that would not survive being written into a `q` — is a `400`
//! naming what was wrong.
//!
//! A predicate is never dropped and never half applied. That is the property worth attacking:
//! the compiled `q` goes to the evaluator as the caller's own filter, and an unbalanced one is
//! dropped there (`conjoin`, `is_balanced`) — so a filter this compiler lets through malformed
//! comes back as rows the caller asked not to see, with nothing said about it (T-2348). The
//! depth guard in the module's own unit tests covers the recursion; these are the inputs.

use context_gateway::translators::cql2::compile;

fn q(filter: &str) -> String {
    compile(filter)
        .unwrap_or_else(|error| panic!("{filter:?} is in the subset: {error}"))
        .q
        .unwrap_or_else(|| panic!("{filter:?} compiled to no q"))
}

fn refused(filter: &str) -> String {
    let error = compile(filter).expect_err(&format!("{filter:?} is not in the subset"));
    assert_eq!(error.parameter, "filter", "{filter:?}");
    error.detail
}

/// T-2348: a quoted identifier is written into the `q` without quotes of its own, so a name
/// carrying the query language's own punctuation would be structure rather than a name.
#[test]
fn an_identifier_that_would_become_structure_is_refused() {
    for filter in [
        r#""a)|(b" > 5"#,
        r#""a;b" > 5"#,
        r#""a|b" > 5"#,
        r#""(" > 5"#,
        r#""a b" > 5"#,
        r#""a=b" > 5"#,
        r#""" > 5"#,
        r#""a~=x" > 5"#,
        r#"pm10 > 5 AND "x)|(y" = 'z'"#,
    ] {
        refused(filter);
    }

    // A name that is merely unusual is still a name: the set is the one a bare word may carry.
    assert_eq!(q(r#""pm10" > 5"#), "pm10>5");
    assert_eq!(q(r#""urn:ngsi-ld:x" > 5"#), "urn:ngsi-ld:x>5");
    assert_eq!(q(r#""dateObserved.value" > 5"#), "dateObserved.value>5");
    assert_eq!(q(r#""ovzdušie" > 5"#), "ovzdušie>5");
}

/// And a value is written between double quotes, so a value carrying one would close the string
/// early and turn its own tail into structure.
#[test]
fn a_value_carrying_a_double_quote_is_refused() {
    for filter in [
        r#"pm10 = 'a"b'"#,
        r#"pm10 = '"'"#,
        r#"name LIKE 'a"%'"#,
        r#"name IN ('a"b', 'c')"#,
    ] {
        assert_eq!(
            refused(filter),
            "a value may not contain a double quote",
            "{filter}"
        );
    }

    // The other punctuation stays inside the quoted value, where it is data.
    assert_eq!(q("pm10 = 'a);(b'"), r#"pm10=="a);(b""#);
    assert_eq!(q("pm10 = 'a|b'"), r#"pm10=="a|b""#);
}

/// Whatever comes out is a filter the evaluator will conjoin rather than drop: balanced
/// parentheses, closed strings, and the caller's whole condition.
#[test]
fn everything_that_compiles_is_a_balanced_filter() {
    for filter in [
        "pm10 > 50",
        "pm10 >= 50 AND pm25 < 10",
        "pm10 > 50 OR pm25 > 20",
        "(pm10 > 50 OR pm25 > 20) AND dateObserved > '2026-09-18T00:00:00Z'",
        "name LIKE 'Stanica%'",
        "pm10 BETWEEN 10 AND 20",
        "type IN ('a', 'b', 'c')",
        "pm10 IS NULL",
        "pm10 IS NOT NULL",
        "NOT pm10 > 50",
    ] {
        let compiled = q(filter);
        let mut depth = 0i32;
        let mut quoted = false;
        for character in compiled.chars() {
            match character {
                '"' => quoted = !quoted,
                '(' if !quoted => depth += 1,
                ')' if !quoted => depth -= 1,
                _ => {}
            }
            assert!(depth >= 0, "{filter:?} -> {compiled:?}");
        }
        assert_eq!(depth, 0, "{filter:?} -> {compiled:?}");
        assert!(!quoted, "{filter:?} -> {compiled:?} leaves a string open");
    }
}

/// The scalar operators of the subset, each as NGSI-LD writes it.
#[test]
fn every_scalar_operator_of_the_subset_has_its_ngsi_ld_form() {
    for (filter, expected) in [
        ("pm10 = 5", "pm10==5"),
        ("pm10 <> 5", "pm10!=5"),
        ("pm10 != 5", "pm10!=5"),
        ("pm10 < 5", "pm10<5"),
        ("pm10 <= 5", "pm10<=5"),
        ("pm10 > 5", "pm10>5"),
        ("pm10 >= 5", "pm10>=5"),
        ("pm10 BETWEEN 1 AND 2", "pm10==1..2"),
        ("NOT (pm10 BETWEEN 1 AND 2)", "pm10!=1..2"),
        ("pm10 IS NULL", "!pm10"),
        ("pm10 IS NOT NULL", "pm10"),
        ("name = 'Sásová'", "name==\"Sásová\""),
        ("name LIKE 'Sás%'", "name~=\"^Sás.*$\""),
    ] {
        assert_eq!(q(filter), expected, "{filter}");
    }
}

/// A `LIKE` pattern is anchored and every character but the two SQL wildcards is matched
/// literally, so a pattern carrying regular expression syntax cannot widen itself.
#[test]
fn a_like_pattern_cannot_widen_itself_into_a_regular_expression() {
    for (pattern, expected) in [
        ("a.b", r#"name~="^a\.b$""#),
        ("a*b", r#"name~="^a\*b$""#),
        (".*", r#"name~="^\.\*$""#),
        ("a+b", r#"name~="^a\+b$""#),
        ("a$", r#"name~="^a\$$""#),
        ("a%", r#"name~="^a.*$""#),
        ("a_b", r#"name~="^a.b$""#),
    ] {
        assert_eq!(q(&format!("name LIKE '{pattern}'")), expected, "{pattern}");
    }
}

/// An operator the endpoint does not implement is named, so a client can tell its user which
/// predicate to remove rather than guessing.
#[test]
fn an_operator_outside_the_subset_is_named() {
    for (filter, named) in [
        ("NOT name LIKE 'a%'", "NOT LIKE"),
        ("S_CROSSES(location, POINT(1 2))", "S_CROSSES"),
        (
            "T_EQUALS(dateObserved, TIMESTAMP('2026-09-18T00:00:00Z'))",
            "T_EQUALS",
        ),
        ("ACCENTI(name) = 'a'", "ACCENTI"),
        ("CASEI(name) = 'a'", "CASEI"),
    ] {
        let detail = refused(filter);
        assert!(detail.contains(named), "{filter} -> {detail}");
    }
}

/// A spatial or temporal predicate is one per query, because NGSI-LD carries one of each; a
/// second one is refused rather than quietly replacing the first.
#[test]
fn a_second_spatial_or_temporal_predicate_is_refused() {
    let polygon = "POLYGON((19.1 48.7,19.2 48.7,19.2 48.8,19.1 48.7))";
    let one = format!("S_INTERSECTS(location, {polygon})");
    assert_eq!(compile(&one).expect("one spatial predicate").geo.len(), 3);

    let two = format!("{one} AND {one}");
    assert!(refused(&two).contains("only one spatial"), "{two}");

    let during = "T_DURING(dateObserved, INTERVAL('2026-09-18T00:00:00Z','2026-09-19T00:00:00Z'))";
    assert_eq!(
        compile(during)
            .expect("one temporal predicate")
            .temporal
            .len(),
        4
    );
    assert!(refused(&format!("{during} AND {during}")).contains("only one temporal"));
}

/// A spatial or temporal predicate under an `OR` cannot be expressed at all, and is refused
/// rather than applied to the whole query — which would narrow what the caller wanted widened.
#[test]
fn a_spatial_predicate_under_an_or_is_refused() {
    let polygon = "POLYGON((19.1 48.7,19.2 48.7,19.2 48.8,19.1 48.7))";
    let filter = format!("pm10 > 5 OR S_INTERSECTS(location, {polygon})");
    assert!(refused(&filter).contains("OR"), "{filter}");
}

/// An empty or unclosed filter is a client mistake and is named as one.
#[test]
fn an_empty_or_unclosed_filter_is_refused() {
    for filter in [
        "",
        "   ",
        "'unclosed",
        "\"unclosed",
        "(pm10 > 5",
        "pm10 > 5)",
        "AND",
        "()",
    ] {
        refused(filter);
    }
}

/// A timestamp in a temporal predicate has to be one, and a date is the start of its day.
#[test]
fn a_temporal_predicate_takes_rfc3339_and_nothing_else() {
    let good = "T_AFTER(dateObserved, TIMESTAMP('2026-09-18T00:00:00Z'))";
    assert_eq!(
        compile(good).expect("a temporal predicate").temporal,
        vec![
            ("timerel".to_owned(), "after".to_owned()),
            ("timeAt".to_owned(), "2026-09-18T00:00:00Z".to_owned()),
            // The attribute the predicate names travels with it: a temporal query over
            // `dateObserved` is not the same question as one over `observedAt`.
            ("timeproperty".to_owned(), "dateObserved".to_owned()),
        ]
    );
    assert_eq!(
        compile("T_AFTER(dateObserved, DATE('2026-09-18'))")
            .expect("a date")
            .temporal[1]
            .1,
        "2026-09-18T00:00:00Z",
        "a day is its start"
    );
    // `TIMESTAMP` and `DATE` are the same door: a ten-character date is read as its start
    // whichever of them a client wrapped it in, rather than refused on the wrapper's name.
    assert_eq!(
        compile("T_AFTER(dateObserved, TIMESTAMP('2026-09-18'))")
            .expect("a date under either name")
            .temporal[1]
            .1,
        "2026-09-18T00:00:00Z"
    );
    for bad in [
        "T_AFTER(dateObserved, TIMESTAMP('yesterday'))",
        "T_AFTER(dateObserved, TIMESTAMP('2026-13-01T00:00:00Z'))",
    ] {
        refused(bad);
    }
}

/// Case and whitespace are the client's; the keywords are read in any of them and the compiled
/// filter is the same.
#[test]
fn keywords_are_read_in_any_case_and_spacing() {
    let expected = q("pm10 > 50 AND pm25 < 10");
    for filter in [
        "pm10>50 and pm25<10",
        "pm10  >  50   AnD   pm25  <  10",
        "\tpm10 > 50\nAND pm25 < 10\t",
    ] {
        assert_eq!(q(filter), expected, "{filter}");
    }
}

/// The same filter compiles the same way twice, and a long conjunction keeps every one of its
/// terms — a filter that loses a term returns rows the caller asked not to see.
#[test]
fn every_term_of_a_long_conjunction_survives() {
    let terms: Vec<String> = (0..64)
        .map(|index| format!("attr{index:02} > {index}"))
        .collect();
    let filter = terms.join(" AND ");
    let compiled = q(&filter);

    for index in 0..64 {
        assert!(
            compiled.contains(&format!("attr{index:02}>{index}")),
            "term {index} is missing"
        );
    }
    assert_eq!(compiled, q(&filter));
}
