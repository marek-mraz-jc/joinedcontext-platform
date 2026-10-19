//! What a stored turn must not keep (AG-99, T-4670, T-4681). Always ([`secrets`]): access
//! tokens, API keys, passwords, private key blocks and payment card numbers, which help no one
//! debug and would be a breach to hold. Under `transcriptRedaction: personal` ([`redact`]) also
//! e-mail addresses, IBANs, Slovak birth numbers and phone numbers. Patterns with a checksum
//! (IBAN, card, birth number) are replaced only when it holds, so an amount, a date or a postcode
//! stays as it was typed.

use std::sync::LazyLock;

use regex::{Captures, Regex};

static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}")
        .expect("valid regex")
});
static IBAN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Z]{2}\d{2}(?: ?[A-Z0-9]{4}){2,7}(?: ?[A-Z0-9]{1,3})?\b").expect("valid regex")
});
static CARD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d(?:[ -]?\d){12,18}\b").expect("valid regex"));
static BIRTH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d{6}) ?/? ?(\d{3,4})\b").expect("valid regex"));
static PHONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\+|\b00)[1-9]\d{0,2}[ /-]?\d{2,4}(?:[ /-]?\d{2,4}){1,4}\b|\b0\d{1,3}[ /-]?\d{2,4}(?:[ -]?\d{2,4}){1,2}\b")
        .expect("valid regex")
});

static PRIVATE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
    )
    .expect("valid regex")
});
/// A bearer or basic credential, a JWT, and the key shapes of the common providers (`sk-…`,
/// GitHub, Slack, AWS, Google); each at least 16 characters past its prefix.
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i:\b(?:bearer|basic|token)\s+)[A-Za-z0-9._~+/=-]{16,}",
        r"|\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
        r"|\b(?:sk|pk|rk)-(?:[A-Za-z0-9]+-)*[A-Za-z0-9_]{16,}",
        r"|\b(?:ghp|gho|ghu|ghs|ghr|github_pat|glpat|xox[abpors])[-_][A-Za-z0-9_-]{16,}",
        r"|\bAKIA[0-9A-Z]{16}\b|\bAIza[0-9A-Za-z_-]{35}"
    ))
    .expect("valid regex")
});
/// A password after its word: `heslo: x`, `password=x`, `moje heslo je x`.
static PASSWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(heslo|hesl[oa]m|password|passwd|pwd|pass|kód PIN|PIN)(\s*[:=]\s*|\s+(?:je|is|znie)\s+)(\S+)")
        .expect("valid regex")
});

/// `text` with every private key, access token, API key, password and card number replaced:
/// what every stored turn has masked (T-4681).
pub fn secrets(text: &str) -> String {
    let text = PRIVATE_KEY.replace_all(text, "[kľúč]");
    let text = TOKEN.replace_all(&text, "[token]");
    let text = PASSWORD.replace_all(&text, "$1$2[heslo]");
    CARD.replace_all(&text, |c: &Captures| keep_unless(&c[0], luhn, "[karta]"))
        .into_owned()
}

/// `text` as a deployment of `transcriptRedaction` `personal` (or not) stores it.
pub fn masked(text: &str, personal: bool) -> String {
    if personal {
        redact(&secrets(text))
    } else {
        secrets(text)
    }
}

/// `text` with every e-mail address, IBAN, card number, birth number and phone number replaced.
pub fn redact(text: &str) -> String {
    let text = EMAIL.replace_all(text, "[e-mail]");
    let text = IBAN.replace_all(&text, |c: &Captures| keep_unless(&c[0], iban, "[IBAN]"));
    let text = CARD.replace_all(&text, |c: &Captures| keep_unless(&c[0], luhn, "[karta]"));
    let text = BIRTH.replace_all(&text, |c: &Captures| {
        if birth_number(&c[1], &c[2]) {
            "[rodné číslo]".to_owned()
        } else {
            c[0].to_owned()
        }
    });
    PHONE
        .replace_all(&text, |c: &Captures| keep_unless(&c[0], phone, "[telefón]"))
        .into_owned()
}

/// A phone number: 9 or 10 digits from a leading 0, 10 to 15 with the country code.
fn phone(found: &str) -> bool {
    let digits = found.chars().filter(char::is_ascii_digit).count();
    let international = found.starts_with('+')
        || found
            .strip_prefix("00")
            .is_some_and(|rest| rest.starts_with(|c: char| ('1'..='9').contains(&c)));
    if international {
        (10..=17).contains(&digits)
    } else {
        (9..=10).contains(&digits)
    }
}

fn keep_unless(found: &str, holds: fn(&str) -> bool, placeholder: &str) -> String {
    if holds(found) {
        placeholder.to_owned()
    } else {
        found.to_owned()
    }
}

/// ISO 13616: the country and check digits moved to the end, letters as numbers, mod 97 is 1.
fn iban(found: &str) -> bool {
    let compact: String = found.chars().filter(|c| !c.is_whitespace()).collect();
    if !(15..=34).contains(&compact.len()) {
        return false;
    }
    let (head, tail) = compact.split_at(4);
    let mut rest = 0u32;
    for c in tail.chars().chain(head.chars()) {
        let digits = match c.to_digit(36) {
            Some(d) => d,
            None => return false,
        };
        rest = if digits >= 10 {
            (rest * 100 + digits) % 97
        } else {
            (rest * 10 + digits) % 97
        };
    }
    rest == 1
}

/// A card number: 13 to 19 digits whose Luhn sum is a multiple of 10.
fn luhn(found: &str) -> bool {
    let digits: Vec<u32> = found.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| match (i % 2, d * 2) {
            (1, doubled) if doubled > 9 => doubled - 9,
            (1, doubled) => doubled,
            _ => d,
        })
        .sum();
    sum.is_multiple_of(10)
}

/// A Slovak (or Czech) birth number: `YYMMDD` with the month plus 50 for a woman (plus 20 or 70
/// since 2004) and a real day, then three digits (born before 1954) or four, the whole ten digits
/// a multiple of 11 (or ending in 0 when the rest leaves 10).
fn birth_number(date: &str, suffix: &str) -> bool {
    let number = |s: &str| s.parse::<u64>().ok();
    let (Some(month), Some(day)) = (number(&date[2..4]), number(&date[4..6])) else {
        return false;
    };
    let month = match month {
        1..=12 => month,
        21..=32 => month - 20,
        51..=62 => month - 50,
        71..=82 => month - 70,
        _ => return false,
    };
    if !(1..=31).contains(&day) || !(1..=12).contains(&month) {
        return false;
    }
    if suffix.len() == 3 {
        return true;
    }
    let (Some(head), Some(whole)) = (
        number(&format!("{date}{}", &suffix[..3])),
        number(&format!("{date}{suffix}")),
    ) else {
        return false;
    };
    whole % 11 == 0 || (head % 11 == 10 && whole % 10 == 0)
}

#[cfg(test)]
mod tests {
    use super::{masked, redact, secrets};

    #[test]
    fn every_secret_is_masked_and_the_rest_stays_verbatim() {
        let key = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\n-----END PRIVATE KEY-----";
        assert_eq!(secrets(&format!("kľúč {key} koniec")), "kľúč [kľúč] koniec");
        assert_eq!(
            secrets("-----BEGIN RSA PRIVATE KEY-----\nMIIE useknuté"),
            "[kľúč]"
        );
        assert_eq!(
            secrets("Authorization: Bearer abcDEF0123456789xyz.QRS"),
            "Authorization: [token]"
        );
        assert_eq!(
            secrets(
                "token eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N"
            ),
            "[token]"
        );
        assert_eq!(
            secrets("kľúč sk-or-v1-0123456789abcdef0123"),
            "kľúč [token]"
        );
        assert_eq!(secrets("ghp_0123456789abcdefABCDEF0123"), "[token]");
        assert_eq!(secrets("AKIAIOSFODNN7EXAMPLE"), "[token]");
        assert_eq!(secrets("heslo: Tajne123!"), "heslo: [heslo]");
        assert_eq!(secrets("moje heslo je mačka7"), "moje heslo je [heslo]");
        assert_eq!(
            secrets("password=hunter2 a dosť"),
            "password=[heslo] a dosť"
        );
        assert_eq!(secrets("karta 4111 1111 1111 1111"), "karta [karta]");
    }

    #[test]
    fn secrets_alone_keep_an_address_a_phone_and_an_iban_as_typed() {
        let text = "Som Ján, jan@mesto.sk, 0905 123 456, IBAN SK31 1200 0000 1987 4263 7541. Kedy je odvoz?";
        assert_eq!(secrets(text), text);
        assert_eq!(masked(text, false), text);
        assert_eq!(
            masked(text, true),
            "Som Ján, [e-mail], [telefón], IBAN [IBAN]. Kedy je odvoz?"
        );
        for text in [
            "Token pre API?",
            "heslo",
            "Aké je heslo k wifi v knižnici?",
            "sk-12",
            "4111 1111 1111 1112",
        ] {
            assert_eq!(secrets(text), text);
        }
        assert_eq!(
            masked("heslo: x9 a a@b.sk", true),
            "heslo: [heslo] a [e-mail]"
        );
    }

    #[test]
    fn each_pattern_is_replaced() {
        assert_eq!(
            redact("píšte na jan.novak@mesto.sk, prosím"),
            "píšte na [e-mail], prosím"
        );
        assert_eq!(redact("IBAN SK31 1200 0000 1987 4263 7541"), "IBAN [IBAN]");
        assert_eq!(redact("IBAN SK3112000000198742637541."), "IBAN [IBAN].");
        assert_eq!(redact("karta 4111 1111 1111 1111"), "karta [karta]");
        assert_eq!(redact("karta 4111-1111-1111-1111"), "karta [karta]");
        assert_eq!(redact("tel. 0905 123 456"), "tel. [telefón]");
        assert_eq!(redact("tel. +421 905 123 456"), "tel. [telefón]");
        assert_eq!(redact("volajte 048/415 5111"), "volajte [telefón]");
        assert_eq!(redact("00421905123456"), "[telefón]");
    }

    #[test]
    fn a_birth_number_with_and_without_the_slash() {
        // 736028/5163: a woman born 28. 10. 1973, divisible by 11.
        assert_eq!(redact("RČ 736028/5163"), "RČ [rodné číslo]");
        assert_eq!(redact("RČ 7360285163"), "RČ [rodné číslo]");
        // Born before 1954: three digits after the slash.
        assert_eq!(redact("RČ 520101/123"), "RČ [rodné číslo]");
    }

    #[test]
    fn dates_postcodes_amounts_and_wrong_checksums_stay() {
        for text in [
            "2026-10-18",
            "18. 10. 2026",
            "974 01 Banská Bystrica",
            "PSČ 97401",
            "rozpočet 1 234 567 eur",
            "PM10 je 21,5 µg/m³",
            "o 18:00",
            "736028/5164",
            "4111 1111 1111 1112",
            "SK31 1200 0000 1987 4263 7542",
            "991332/1234",
            "zmluva č. 2024/1234",
        ] {
            assert_eq!(redact(text), text);
        }
    }

    #[test]
    fn several_in_one_question_and_text_around_them_kept() {
        assert_eq!(
            redact("Som Ján, a@b.sk, 0905 123 456. Kedy je odvoz?"),
            "Som Ján, [e-mail], [telefón]. Kedy je odvoz?"
        );
    }
}
