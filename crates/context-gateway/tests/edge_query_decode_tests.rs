//! Edge cases of `query::decode` (T-1946, T-2338, EP-26, MP-02).
//!
//! Contract, in one sentence: it reverses percent-encoding and the form encoding's `+` byte by
//! byte, leaves anything that is not a complete escape as the literal characters the caller sent,
//! replaces bytes that are not valid UTF-8 rather than dropping or refusing them — and never
//! panics, whatever the caller writes.
//!
//! The last clause is the security clause and it was not true until T-2338: the two hex digits
//! used to be sliced out of the string by a byte index, so `%` before a multi-byte character cut
//! that character in half and panicked the handler. `decode` runs on every query parameter
//! (`query::parse`), on an attribute name inside `q` a second time (`query.rs`'s `compact`) and on
//! the entity id in the path (`app.rs`), so one anonymous GET reached it.

use context_gateway::query::{decode, encode, parse, referenced_attributes};

/// T-2338: a percent sign whose next two bytes are not a whole character. Every one of these
/// panicked before the fix; none of them is an escape, so each is the literal text.
#[test]
fn a_percent_before_a_multibyte_character_does_not_panic() {
    for (raw, decoded) in [
        ("%a\u{20AC}x", "%a\u{20AC}x"),
        ("%\u{20AC}", "%\u{20AC}"),
        ("a%b\u{20AC}c", "a%b\u{20AC}c"),
        ("%1\u{e9}", "%1\u{e9}"),
        ("%\u{1F600}", "%\u{1F600}"),
        ("%a\u{20AC}%a\u{20AC}", "%a\u{20AC}%a\u{20AC}"),
        ("Bansk\u{e1}%a\u{20AC}", "Bansk\u{e1}%a\u{20AC}"),
    ] {
        assert_eq!(decode(raw), decoded, "{raw:?}");
    }
}

/// And the way a caller reaches it: one query string, decoded once by `parse` and a second time
/// by the reference check that keeps a filter off an attribute the endpoint does not serve.
#[test]
fn a_hostile_query_string_reaches_the_reference_check_without_panicking() {
    let params = parse("q=%25a%E2%82%ACx==1&attrs=%25b%E2%82%AC&orderBy=%25c%E2%82%AC");
    assert_eq!(params[0], ("q".to_owned(), "%a\u{20AC}x==1".to_owned()));

    let names = referenced_attributes(&params);
    assert!(names.contains("%a\u{20AC}x"), "{names:?}");
    assert!(names.contains("%b\u{20AC}"), "{names:?}");
    assert!(names.contains("%c\u{20AC}"), "{names:?}");
}

/// A complete escape is the byte it names, in either case of hexadecimal.
#[test]
fn a_complete_escape_is_the_byte_it_names() {
    for (raw, decoded) in [
        ("%20", " "),
        ("%2F", "/"),
        ("%2f", "/"),
        ("%3B", ";"),
        ("%3b", ";"),
        ("urn%3Angsi-ld%3AVehicle", "urn:ngsi-ld:Vehicle"),
        ("%41%42%43", "ABC"),
    ] {
        assert_eq!(decode(raw), decoded, "{raw:?}");
    }
}

/// An escape that is not one is the literal text: a caller who writes a per-cent sign gets a
/// per-cent sign back, and nothing is swallowed.
#[test]
fn an_incomplete_or_invalid_escape_is_literal() {
    for raw in [
        "%", "%4", "100%", "%%", "%zz", "%g1", "% 1", "%-1", "%_a", "a%", "%\n",
    ] {
        assert_eq!(decode(raw), raw, "{raw:?}");
    }
}

/// `+` is the form encoding's space, and a `+` the caller meant literally arrives escaped.
#[test]
fn plus_is_a_space_and_an_escaped_plus_is_a_plus() {
    assert_eq!(decode("a+b"), "a b");
    assert_eq!(decode("a%2Bb"), "a+b");
    assert_eq!(decode("+"), " ");
    assert_eq!(decode("%2B%2B"), "++");
}

/// Decoding once is decoding once: a double-encoded value comes back still encoded, so nothing
/// the caller wrote is read as structure it did not have.
#[test]
fn decoding_once_is_not_decoding_twice() {
    assert_eq!(decode("%2520"), "%20");
    assert_eq!(decode("%252F"), "%2F");
    assert_eq!(decode("a%253Db"), "a%3Db");
}

/// A multi-byte character arrives as its escapes and comes back whole.
#[test]
fn a_utf8_sequence_survives_its_escapes() {
    assert_eq!(decode("Bansk%C3%A1%20Bystrica"), "Banská Bystrica");
    assert_eq!(decode("%E2%82%AC"), "\u{20AC}");
    assert_eq!(decode("%F0%9F%98%80"), "\u{1F600}");
}

/// Escapes that are not valid UTF-8 are replaced rather than refused or dropped: the caller gets
/// a string, the gateway gets no panic, and nothing downstream sees a byte that is not a
/// character.
#[test]
fn bytes_that_are_not_valid_utf8_are_replaced() {
    for raw in ["%FF", "%FE%FF", "%C3", "%C3%28", "%ED%A0%80"] {
        let decoded = decode(raw);
        assert!(!decoded.is_empty(), "{raw:?}");
        assert!(decoded.contains('\u{FFFD}'), "{raw:?} -> {decoded:?}");
    }
}

/// A control character the caller escaped is decoded as written; what may not carry it is the
/// header or the log line downstream, which is where it is refused, not here.
#[test]
fn control_characters_are_decoded_as_written() {
    assert_eq!(decode("%00"), "\u{0}");
    assert_eq!(decode("%0D%0A"), "\r\n");
    assert_eq!(decode("a%0D%0ASet-Cookie:%20b"), "a\r\nSet-Cookie: b");
    assert_eq!(decode("%09"), "\t");
}

/// Nothing in, nothing out.
#[test]
fn an_empty_string_decodes_to_an_empty_string() {
    assert_eq!(decode(""), "");
    assert_eq!(decode("="), "=");
}

/// Every byte value survives the round trip, which is what makes `encode` safe to use on a whole
/// URL inside one parameter (the egress path does exactly that).
#[test]
fn every_byte_round_trips_through_encode_and_decode() {
    for byte in 0..=255u8 {
        let raw = format!("%{byte:02X}");
        let decoded = decode(&raw);
        let bytes = decoded.as_bytes();
        if byte.is_ascii() {
            assert_eq!(bytes, [byte], "{raw}");
            assert_eq!(decode(&encode(&decoded)), decoded, "{raw}");
        } else {
            assert_eq!(decoded, "\u{FFFD}", "{raw}");
        }
    }

    let text = "urn:ngsi-ld:Vehicle:hel.fi:fleet:1 Banská €";
    assert_eq!(decode(&encode(text)), text);
}

/// A long input is decoded whole: the last escape of a query a caller pushed to the limit is read
/// like the first.
#[test]
fn a_long_value_is_decoded_to_its_last_escape() {
    let raw = "%41".repeat(4096) + "%E2%82%AC";
    let decoded = decode(&raw);
    assert_eq!(decoded.chars().filter(|c| *c == 'A').count(), 4096);
    assert!(decoded.ends_with('\u{20AC}'));
}
