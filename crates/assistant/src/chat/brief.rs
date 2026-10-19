//! Messages answered before the router (T-4768): small talk ("Ahoj, ako sa máš?", "ďakujem",
//! "s ničím") gets one fixed line in the message's language, and a message with no content word
//! ("b", "ddd", "?") is asked what the person looks for, with the deployment's example questions
//! as chips. Neither searches, reads data or calls a model: a single letter once retrieved an
//! unrelated answer on another city's assistant.
use super::agent::{Ask, Event};
use std::time::Instant;
use tokio::sync::mpsc::Sender;

/// What kind of short message it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Brief {
    /// "ako sa máš", "how are you": answered, then asked what the person needs.
    HowAreYou,
    /// "ahoj", "dobrý deň".
    Greeting,
    /// "ďakujem", "thanks".
    Thanks,
    /// "s ničím", "dobre", "ok", "bye": a short closing line.
    Closing,
    /// No content word: 1-2 characters, one letter repeated, punctuation only.
    Clarify,
}

impl Brief {
    /// The route the turn's trace and transcript record.
    pub fn route(self) -> &'static str {
        match self {
            Brief::Clarify => "clarify",
            _ => "small-talk",
        }
    }
}

/// Small-talk phrases, diacritics folded and lowercased, each with its kind and the language only
/// it belongs to (`None` when Slovak and Czech share it, or it is international).
const PHRASES: &[(&str, Brief, Option<&str>)] = &[
    ("ako sa mas", Brief::HowAreYou, Some("sk")),
    ("ako sa mate", Brief::HowAreYou, Some("sk")),
    ("ako sa dari", Brief::HowAreYou, Some("sk")),
    ("ako sa ti dari", Brief::HowAreYou, Some("sk")),
    ("ako sa vam dari", Brief::HowAreYou, Some("sk")),
    ("jak se mas", Brief::HowAreYou, Some("cs")),
    ("jak se mate", Brief::HowAreYou, Some("cs")),
    ("jak se dari", Brief::HowAreYou, Some("cs")),
    ("how are you", Brief::HowAreYou, Some("en")),
    ("how are you doing", Brief::HowAreYou, Some("en")),
    ("how is it going", Brief::HowAreYou, Some("en")),
    ("whats up", Brief::HowAreYou, Some("en")),
    ("what s up", Brief::HowAreYou, Some("en")),
    ("wie gehts", Brief::HowAreYou, Some("de")),
    ("wie geht s", Brief::HowAreYou, Some("de")),
    ("wie geht es dir", Brief::HowAreYou, Some("de")),
    ("wie geht es ihnen", Brief::HowAreYou, Some("de")),
    ("hogy vagy", Brief::HowAreYou, Some("hu")),
    ("hogy van", Brief::HowAreYou, Some("hu")),
    ("як справи", Brief::HowAreYou, Some("uk")),
    ("ahoj", Brief::Greeting, None),
    ("ahojte", Brief::Greeting, None),
    ("cau", Brief::Greeting, None),
    ("cauko", Brief::Greeting, Some("sk")),
    ("nazdar", Brief::Greeting, None),
    ("zdravim", Brief::Greeting, None),
    ("zdravime", Brief::Greeting, None),
    ("dobry den", Brief::Greeting, None),
    ("dobre rano", Brief::Greeting, None),
    ("dobry vecer", Brief::Greeting, None),
    ("dobre popoludnie", Brief::Greeting, Some("sk")),
    ("dobre odpoledne", Brief::Greeting, Some("cs")),
    ("hello", Brief::Greeting, Some("en")),
    ("hi", Brief::Greeting, Some("en")),
    ("hey", Brief::Greeting, Some("en")),
    ("good morning", Brief::Greeting, Some("en")),
    ("good afternoon", Brief::Greeting, Some("en")),
    ("good evening", Brief::Greeting, Some("en")),
    ("hallo", Brief::Greeting, Some("de")),
    ("guten tag", Brief::Greeting, Some("de")),
    ("guten morgen", Brief::Greeting, Some("de")),
    ("servus", Brief::Greeting, None),
    ("szia", Brief::Greeting, Some("hu")),
    ("jo napot", Brief::Greeting, Some("hu")),
    ("привіт", Brief::Greeting, Some("uk")),
    ("добрий день", Brief::Greeting, Some("uk")),
    ("dakujem", Brief::Thanks, Some("sk")),
    ("dakujeme", Brief::Thanks, Some("sk")),
    ("dakujem pekne", Brief::Thanks, Some("sk")),
    ("dakujem velmi pekne", Brief::Thanks, Some("sk")),
    ("vdaka", Brief::Thanks, Some("sk")),
    ("dik", Brief::Thanks, None),
    ("diky", Brief::Thanks, None),
    ("dekuji", Brief::Thanks, Some("cs")),
    ("dekuju", Brief::Thanks, Some("cs")),
    ("dekuji moc", Brief::Thanks, Some("cs")),
    ("thanks", Brief::Thanks, Some("en")),
    ("thank you", Brief::Thanks, Some("en")),
    ("thank you very much", Brief::Thanks, Some("en")),
    ("thanks a lot", Brief::Thanks, Some("en")),
    ("thx", Brief::Thanks, Some("en")),
    ("danke", Brief::Thanks, Some("de")),
    ("danke schon", Brief::Thanks, Some("de")),
    ("koszonom", Brief::Thanks, Some("hu")),
    ("koszi", Brief::Thanks, Some("hu")),
    ("дякую", Brief::Thanks, Some("uk")),
    ("s nicim", Brief::Closing, Some("sk")),
    ("nicim", Brief::Closing, Some("sk")),
    ("nic", Brief::Closing, None),
    ("nic nepotrebujem", Brief::Closing, Some("sk")),
    ("nic nepotrebuji", Brief::Closing, Some("cs")),
    ("to je vsetko", Brief::Closing, Some("sk")),
    ("to je vse", Brief::Closing, Some("cs")),
    ("dobre", Brief::Closing, None),
    ("fajn", Brief::Closing, None),
    ("super", Brief::Closing, None),
    ("ok", Brief::Closing, None),
    ("okay", Brief::Closing, None),
    ("okej", Brief::Closing, None),
    ("v poriadku", Brief::Closing, Some("sk")),
    ("v pohode", Brief::Closing, None),
    ("v poradku", Brief::Closing, Some("cs")),
    ("jasne", Brief::Closing, None),
    ("dovidenia", Brief::Closing, Some("sk")),
    ("na shledanou", Brief::Closing, Some("cs")),
    ("majte sa", Brief::Closing, Some("sk")),
    ("mejte se", Brief::Closing, Some("cs")),
    ("nothing", Brief::Closing, Some("en")),
    ("nothing else", Brief::Closing, Some("en")),
    ("thats all", Brief::Closing, Some("en")),
    ("that s all", Brief::Closing, Some("en")),
    ("fine", Brief::Closing, Some("en")),
    ("bye", Brief::Closing, Some("en")),
    ("goodbye", Brief::Closing, Some("en")),
    ("tschuss", Brief::Closing, Some("de")),
    ("viszlat", Brief::Closing, Some("hu")),
];

/// Words that may stand between the phrases of a small-talk message: "Ďakujem, a vy?".
const FILLERS: &[&str] = &[
    "a", "aj", "ty", "vy", "ti", "vam", "tebe", "and", "you", "too", "ja", "tiez", "taky",
    "prosim", "please", "moc", "velmi", "pekne",
];

/// The message's kind and the language its phrases name, when it is small talk or noise; `None`
/// for a message the router reads.
pub fn brief(question: &str) -> Option<(Brief, Option<&'static str>)> {
    let folded = fold(question);
    let words: Vec<&str> = folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    // Small talk first: "hi" and "ok" are short but say something.
    small_talk(&words).or_else(|| {
        words
            .iter()
            .all(|w| noise(w))
            .then_some((Brief::Clarify, None))
    })
}

/// A word that carries nothing: one or two characters, or one character repeated ("ddd").
fn noise(word: &str) -> bool {
    let mut chars = word.chars();
    let first = chars.next();
    word.chars().count() <= 2 || chars.all(|c| Some(c) == first)
}

/// The words read as small-talk phrases and fillers only, longest phrase first, at least one
/// phrase among them; the kind is the strongest one met (how-are-you, greeting, thanks, closing).
fn small_talk(words: &[&str]) -> Option<(Brief, Option<&'static str>)> {
    let mut at = 0;
    let mut kind: Option<Brief> = None;
    let mut lang = None;
    while at < words.len() {
        let matched = PHRASES
            .iter()
            .filter_map(|(phrase, k, l)| {
                let parts: Vec<&str> = phrase.split(' ').collect();
                words[at..]
                    .starts_with(&parts)
                    .then_some((parts.len(), *k, *l))
            })
            .max_by_key(|(len, ..)| *len);
        match matched {
            Some((len, k, l)) => {
                at += len;
                kind = Some(kind.map_or(k, |old| stronger(old, k)));
                lang = lang.or(l);
            }
            None if FILLERS.contains(&words[at]) => at += 1,
            None => return None,
        }
    }
    kind.map(|k| (k, lang))
}

fn stronger(a: Brief, b: Brief) -> Brief {
    let rank = |k| match k {
        Brief::HowAreYou => 0,
        Brief::Greeting => 1,
        Brief::Thanks => 2,
        _ => 3,
    };
    if rank(b) < rank(a) {
        b
    } else {
        a
    }
}

/// Lowercased, the Latin letters of the six languages without their diacritics.
fn fold(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'ä' => 'a',
            'č' => 'c',
            'ď' => 'd',
            'é' | 'ě' => 'e',
            'í' => 'i',
            'ĺ' | 'ľ' => 'l',
            'ň' => 'n',
            'ó' | 'ô' | 'ö' | 'ő' => 'o',
            'ŕ' | 'ř' => 'r',
            'š' => 's',
            'ť' => 't',
            'ú' | 'ů' | 'ü' | 'ű' => 'u',
            'ý' => 'y',
            'ž' => 'z',
            'ß' => 's',
            '\u{2019}' | '\'' => ' ',
            other => other,
        })
        .collect()
}

/// The fixed reply of `kind` in `lang`; `chips` tells a clarify line that example questions follow.
pub fn reply(kind: Brief, lang: &str, chips: bool) -> String {
    let (how, hello, thanks, closing, clarify, pick) = match lang {
        "en" => (
            "I'm fine, thank you. What can I help you with?",
            "Hello. What can I help you with?",
            "You're welcome. Just write when you look for something else.",
            "All right. Just write when you look for something.",
            "Please write what you are looking for.",
            " Or pick one of the questions below.",
        ),
        "cs" => (
            "Mám se dobře, děkuji. S čím vám mohu pomoci?",
            "Dobrý den. S čím vám mohu pomoci?",
            "Rádo se stalo. Když budete ještě něco hledat, stačí napsat.",
            "Dobře. Když budete něco hledat, stačí napsat.",
            "Napište prosím, co hledáte.",
            " Nebo vyberte jednu z otázek níže.",
        ),
        "de" => (
            "Mir geht es gut, danke. Wobei kann ich Ihnen helfen?",
            "Guten Tag. Wobei kann ich Ihnen helfen?",
            "Gern geschehen. Schreiben Sie einfach, wenn Sie noch etwas suchen.",
            "In Ordnung. Schreiben Sie einfach, wenn Sie etwas suchen.",
            "Bitte schreiben Sie, was Sie suchen.",
            " Oder wählen Sie eine der Fragen unten.",
        ),
        "hu" => (
            "Köszönöm, jól vagyok. Miben segíthetek?",
            "Jó napot. Miben segíthetek?",
            "Szívesen. Írjon, ha még valamit keres.",
            "Rendben. Írjon, ha valamit keres.",
            "Kérem, írja meg, mit keres.",
            " Vagy válasszon az alábbi kérdések közül.",
        ),
        "uk" => (
            "У мене все добре, дякую. Чим я можу допомогти?",
            "Добрий день. Чим я можу допомогти?",
            "Будь ласка. Пишіть, якщо шукатимете ще щось.",
            "Добре. Пишіть, коли щось шукатимете.",
            "Будь ласка, напишіть, що ви шукаєте.",
            " Або виберіть одне з питань нижче.",
        ),
        _ => (
            "Mám sa dobre, ďakujem. S čím vám môžem pomôcť?",
            "Dobrý deň. S čím vám môžem pomôcť?",
            "Rado sa stalo. Keď budete ešte niečo hľadať, stačí napísať.",
            "Dobre. Keď budete niečo hľadať, stačí napísať.",
            "Napíšte, prosím, čo hľadáte.",
            " Alebo vyberte jednu z otázok nižšie.",
        ),
    };
    match kind {
        Brief::HowAreYou => how.to_owned(),
        Brief::Greeting => hello.to_owned(),
        Brief::Thanks => thanks.to_owned(),
        Brief::Closing => closing.to_owned(),
        Brief::Clarify if chips => format!("{clarify}{pick}"),
        Brief::Clarify => clarify.to_owned(),
    }
}

/// The example questions a clarify line offers, in `lang` else the deployment's `first`
/// language: trimmed, none empty, at most [`super::routed::MAX_FOLLOWUPS`]; none for small talk.
fn chips(
    suggestions: &std::collections::BTreeMap<String, Vec<String>>,
    kind: Brief,
    lang: &str,
    first: &str,
) -> Vec<String> {
    if kind != Brief::Clarify {
        return Vec::new();
    }
    suggestions
        .get(lang)
        .or_else(|| suggestions.get(first))
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .take(super::routed::MAX_FOLLOWUPS)
        .collect()
}

/// Answers a [`brief`] message with its fixed line: no search, no read, no model call. A clarify
/// line offers the deployment's example questions in the reply's language as chips.
pub(super) async fn answer(
    ask: &Ask<'_>,
    question: &str,
    kind: Brief,
    named: Option<&str>,
    events: &Sender<Event>,
) {
    let first = ask.languages.first().map_or("sk", String::as_str);
    let lang = super::scope::language(question, named, first);
    let chips = chips(ask.suggestions, kind, lang, first);
    ask.trace
        .note("route", kind.route(), Instant::now(), None, None);
    // A channel that went away stops reading; the turn is over either way.
    let _ = events
        .send(Event::Answer(reply(kind, lang, !chips.is_empty())))
        .await;
    let _ = events.send(Event::Citations(Vec::new())).await;
    if !chips.is_empty() {
        let _ = events.send(Event::Followups(chips)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_asks_and_a_short_real_question_is_routed() {
        for noise in [
            "a", "b", "e", "d", "ddd", "?", "...", "  ", "aaaa", "b?", "5", "x y",
        ] {
            assert_eq!(brief(noise).map(|b| b.0), Some(Brief::Clarify), "{noise:?}");
        }
        for question in [
            "MHD",
            "Kde je most?",
            "ahoj, kde je najbližšia lekáreň?",
            "dobre, a aké sú parkovné zóny?",
            "ok aké sú školy v okrese Zvolen",
            "pm10",
        ] {
            assert_eq!(brief(question), None, "{question:?}");
        }
    }

    #[test]
    fn small_talk_takes_its_strongest_kind_and_the_phrase_s_language() {
        let kind = |q| brief(q).map(|b| b.0);
        assert_eq!(kind("Ahoj ako sa mas?"), Some(Brief::HowAreYou));
        assert_eq!(kind("Ďakujem, a ty?"), Some(Brief::Thanks));
        assert_eq!(kind("s nicim"), Some(Brief::Closing));
        assert_eq!(kind("Dobre, ďakujem"), Some(Brief::Thanks));
        assert_eq!(kind("Hi, how are you?"), Some(Brief::HowAreYou));
        assert_eq!(
            brief("Thank you very much!"),
            Some((Brief::Thanks, Some("en")))
        );
        assert_eq!(brief("dobre"), Some((Brief::Closing, None)));
        assert_eq!(brief("Jak se máš?"), Some((Brief::HowAreYou, Some("cs"))));
    }

    #[test]
    fn chips_come_in_the_reply_s_language_else_the_first_and_only_for_clarify() {
        let suggestions = [
            (
                "sk".to_owned(),
                vec![" Kedy je zber? ".to_owned(), "".to_owned()],
            ),
            (
                "en".to_owned(),
                vec!["a".into(), "b".into(), "c".into(), "d".into()],
            ),
        ]
        .into();
        assert_eq!(
            chips(&suggestions, Brief::Clarify, "de", "sk"),
            ["Kedy je zber?"]
        );
        assert_eq!(chips(&suggestions, Brief::Clarify, "en", "sk").len(), 3);
        assert!(chips(&suggestions, Brief::Greeting, "sk", "sk").is_empty());
    }

    #[test]
    fn the_clarify_line_names_the_chips_only_when_there_are_some() {
        assert_eq!(
            reply(Brief::Clarify, "sk", false),
            "Napíšte, prosím, čo hľadáte."
        );
        assert!(reply(Brief::Clarify, "en", true).ends_with("questions below."));
        assert!(reply(Brief::HowAreYou, "xx", false).starts_with("Mám sa dobre"));
    }
}
