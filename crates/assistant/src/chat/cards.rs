//! What a cited passage itself publishes, as cards the chat shows under the answer (T-4155,
//! API/05 §1.3): a contact, a place, a procedure. Read from the passage's Markdown (its text and
//! its `tel:`/`mailto:` links) without a model call, so a card holds nothing the page does not.

use std::sync::LazyLock;

use regex::Regex;

/// The most phones or e-mails one contact card lists.
const MAX_ITEMS: usize = 5;
/// The most steps one procedure card lists.
const MAX_STEPS: usize = 10;
/// The longest field, in characters; a longer one is cut.
const MAX_CHARS: usize = 300;

/// One card, as the `cards` of a passage citation carries it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Card {
    Contact {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        phones: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        emails: Vec<String>,
    },
    Place {
        address: String,
    },
    Procedure {
        steps: Vec<String>,
    },
}

static TEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(tel:([^)\s]+)\)").unwrap());
static MAILTO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(mailto:([^)\s?]+)").unwrap());
// A Slovak or Czech number as pages write it: `+421 48 4330 111`, `048/4330 111`, `0905 123 456`.
static PHONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[^\w+/])((?:\+42[01]|0)[ /]?\d{2,4}(?:[ /-]?\d{2,4}){1,4})\b").unwrap()
});
static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}").unwrap()
});
// `Československej armády 26, 974 01 Banská Bystrica`: a street and number, a postcode, a town.
static ADDRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\p{Lu}[\p{L}.]*(?: [\p{L}.]+){0,4} \d+[A-Za-z]?(?:/\d+[A-Za-z]?)?, ?\d{3} ?\d{2} \p{Lu}\p{L}+(?: (?:nad |pod )?\p{Lu}\p{L}+){0,3}").unwrap()
});
static STEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(\d{1,2})[.)]\s+(.+)$").unwrap());
static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\([^)]*\)").unwrap());

/// The cards of one passage, contact, place and procedure in that order, each at most once.
pub fn of(text: &str) -> Vec<Card> {
    let mut cards = Vec::new();
    let mut phones: Vec<String> = TEL
        .captures_iter(text)
        .map(|c| c[1].replace("%20", " "))
        .collect();
    let plain = LINK.replace_all(text, "$1");
    phones.extend(
        PHONE
            .captures_iter(&plain)
            .map(|c| c[1].trim().to_owned())
            .filter(|p| national(p).len() == 10),
    );
    let mut emails: Vec<String> = MAILTO
        .captures_iter(text)
        .map(|c| c[1].to_owned())
        .collect();
    emails.extend(EMAIL.find_iter(&plain).map(|m| m.as_str().to_owned()));
    let phones = distinct(phones, national);
    let emails = distinct(emails, |e| e.to_lowercase());
    if !phones.is_empty() || !emails.is_empty() {
        cards.push(Card::Contact {
            name: heading(text),
            phones,
            emails,
        });
    }
    if let Some(address) = ADDRESS.find(&plain) {
        cards.push(Card::Place {
            address: cut(address.as_str()),
        });
    }
    let steps = steps(&plain);
    if steps.len() >= 2 {
        cards.push(Card::Procedure { steps });
    }
    cards
}

/// A number's digits as dialled at home: `+421 48 4330 111` and `048/4330 111` are both
/// `0484330111`.
fn national(phone: &str) -> String {
    let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    match digits
        .strip_prefix("421")
        .or_else(|| digits.strip_prefix("420"))
    {
        Some(rest) if phone.trim_start().starts_with('+') => format!("0{rest}"),
        _ => digits,
    }
}

/// The first run of numbered lines that starts at 1 and counts up by one.
fn steps(text: &str) -> Vec<String> {
    let mut steps = Vec::new();
    for line in text.lines() {
        match STEP.captures(line) {
            Some(c) if c[1].parse() == Ok(steps.len() + 1) => {
                if steps.len() < MAX_STEPS {
                    steps.push(cut(c[2].replace("**", "").trim()));
                }
            }
            _ if steps.len() >= 2 => break,
            _ => steps.clear(),
        }
    }
    steps
}

/// The passage's last heading: the end of its heading path (`# Úrad > ## Kontakt`).
fn heading(text: &str) -> Option<String> {
    let line = text.lines().rfind(|l| l.trim_start().starts_with('#'))?;
    let last = line.rsplit(" > ").next()?;
    let name = LINK.replace_all(last.trim().trim_start_matches('#'), "$1");
    let name = name.trim();
    (!name.is_empty()).then(|| cut(name))
}

/// Each value once, by `key`, at most [`MAX_ITEMS`].
fn distinct(values: Vec<String>, key: impl Fn(&str) -> String) -> Vec<String> {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for value in values {
        let k = key(&value);
        if !k.is_empty() && !seen.contains(&k) && out.len() < MAX_ITEMS {
            seen.push(k);
            out.push(cut(&value));
        }
    }
    out
}

fn cut(text: &str) -> String {
    text.chars().take(MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_contact_page_gives_a_contact_and_a_place() {
        let text = "# Mestský úrad > ## Klientske centrum\n\
            Adresa: Československej armády 26, 974 01 Banská Bystrica\n\
            Tel.: [048/4330 111](tel:+421484330111), 0905 123 456\n\
            E-mail: [podatelna@example.org](mailto:podatelna@example.org), info@example.org\n\
            IČO 00313271, otvorené 8:00 – 16:00, 15. 10. 2026";
        assert_eq!(
            of(text),
            [
                Card::Contact {
                    name: Some("Klientske centrum".into()),
                    phones: vec!["+421484330111".into(), "0905 123 456".into()],
                    emails: vec!["podatelna@example.org".into(), "info@example.org".into()],
                },
                Card::Place {
                    address: "Československej armády 26, 974 01 Banská Bystrica".into()
                },
            ]
        );
    }

    #[test]
    fn numbered_steps_from_one_are_a_procedure() {
        let text =
            "## Ako vybaviť parkovaciu kartu\n1. Vyplňte **žiadosť**.\n2. Zaplaťte poplatok.\n\
            3. Kartu si vyzdvihnite.\nĎalší text.\n1. Iný zoznam.\n2. Druhý.";
        assert_eq!(
            of(text),
            [Card::Procedure {
                steps: vec![
                    "Vyplňte žiadosť.".into(),
                    "Zaplaťte poplatok.".into(),
                    "Kartu si vyzdvihnite.".into()
                ]
            }]
        );
    }

    #[test]
    fn a_passage_without_any_gives_no_card() {
        assert_eq!(of(""), []);
        // Years, a date, an IČO, a postcode alone and one numbered line are none of them.
        assert_eq!(
            of("V roku 2021 a 2022 (IČO 37828100), 974 01.\n1. jediný bod\n3. nie druhý"),
            []
        );
    }

    #[test]
    fn a_contact_without_a_heading_has_no_name_and_repeats_are_dropped() {
        let text = "Volajte +421 48 4330 111 alebo [048 4330 111](tel:048%204330%20111). \
            A@Example.org a a@example.org";
        assert_eq!(
            of(text),
            [Card::Contact {
                name: None,
                phones: vec!["048 4330 111".into()],
                emails: vec!["A@Example.org".into()],
            }]
        );
    }

    #[test]
    fn a_card_serializes_with_its_kind_and_without_empty_fields() {
        let card = Card::Contact {
            name: None,
            phones: vec![],
            emails: vec!["a@example.org".into()],
        };
        assert_eq!(
            serde_json::to_value(&card).unwrap(),
            serde_json::json!({"kind": "contact", "emails": ["a@example.org"]})
        );
    }
}
