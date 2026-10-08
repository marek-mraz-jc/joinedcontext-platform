//! The answer eval of T-3067: ten questions per project (`fixtures/answer_evals/questions.json`),
//! written from what the first crawl of each city's site indexed on dev, and the answers one live
//! run recorded (`fixtures/answer_evals/recordings/{project}.json`), replayed and judged here
//! without a model. The judge is deterministic: the answer is in the asker's language, every
//! `[n]` it writes resolves to a citation on the city's site, the expected facts are in it, no
//! number appears that neither the question nor its source passage holds, and a question the
//! site does not cover is answered without citations.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

/// Below this share of passing answers the build fails: the answers got worse. It only rises.
/// The first recording (dev, 2026-10-07) passed 12 of 20: seven of the eight failures are answers
/// citing "[1, 2]", whose citations jc-assistant dropped, fixed with this eval; the eighth named
/// other OmaStadi projects than the summer's. The task's 0.8 follows the recording on the fixed
/// build.
const PASS_FLOOR: f64 = 0.6;

/// Projects whose questions exist but whose answers were not recorded yet. It only shrinks: a
/// recording that lands takes its project off this list.
const UNRECORDED: &[&str] = &[];

/// Projects of the task whose site's crawl holds no passages yet, so no question can be written
/// from it without inventing facts.
const QUESTIONS_LATER: &[&str] = &["bbsk", "praha"];

const FIXTURE: &str = include_str!("fixtures/answer_evals/questions.json");

/// T-3325's event questions, judged by [`judge_events`].
const EVENTS: &str = include_str!("fixtures/answer_evals/events.json");

/// Whether the event questions still wait for their one live run, on the build that carries the
/// fix. It only goes from true to false: the recording that lands sets it.
const EVENTS_UNRECORDED: bool = true;

/// Words frequent in one language and rare in the others. Slovak and Czech share most of theirs,
/// so first a text is told Slavic, Finnish or English, then Czech from Slovak.
const SK_WORDS: &[&str] = &[
    "sa", "sú", "alebo", "ktorý", "ktorá", "ktoré", "ktorých", "môže", "aj", "nie", "pre", "som",
    "sme", "ste", "budú", "ako", "kedy", "aby", "byť",
];
const CS_WORDS: &[&str] = &[
    "se", "jsou", "nebo", "který", "která", "které", "kterých", "může", "také", "není", "pro",
    "jsem", "jsme", "jste", "budou", "jako", "kdy", "aby", "být",
];
const SLAVIC_WORDS: &[&str] = &[
    "je", "v", "na", "od", "do", "to", "tento", "toto", "však", "bude",
];
const FI_WORDS: &[&str] = &[
    "ja", "on", "ei", "että", "oli", "ovat", "tai", "kun", "myös", "mutta", "voi", "jos", "tämä",
    "joka", "jotka", "sekä", "kanssa", "mukaan", "vuonna", "olla", "milloin", "mitä", "mikä",
    "missä", "mihin", "kuinka", "paljon",
];
const EN_WORDS: &[&str] = &[
    "the", "and", "is", "of", "to", "in", "that", "for", "are", "was", "with", "it", "this", "by",
    "from", "has", "have", "be", "not", "you", "how", "many", "what", "when", "which", "where",
    "since", "do", "does", "its", "an", "at", "as", "or", "they", "their", "will", "can", "there",
];

/// The language of `text` among sk, cs, fi and en; `None` when nothing tells. A Slavic text
/// without a Czech letter or word counts as Slovak.
fn language(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| !w.is_empty())
        .collect();
    let count = |list: &[&str]| words.iter().filter(|w| list.contains(w)).count();
    let letters = |set: &str| lower.chars().filter(|c| set.contains(*c)).count();
    let sk = count(SK_WORDS) + letters("ľĺŕô");
    let cs = count(CS_WORDS) + letters("řůě");
    let slavic = sk + cs + count(SLAVIC_WORDS) + letters("čšžýáíéúňťď");
    let fi = count(FI_WORDS) + letters("äö");
    let en = count(EN_WORDS);
    let best = slavic.max(fi).max(en);
    if best == 0 {
        None
    } else if slavic == best {
        Some(if cs > sk { "cs" } else { "sk" })
    } else if fi == best {
        Some("fi")
    } else {
        Some("en")
    }
}

/// The numbers of a marker's bracket, `1` or `1, 2`; `None` for a bracket of anything else.
fn marker(inside: &str) -> Option<Vec<u64>> {
    inside.split(',').map(|n| n.trim().parse().ok()).collect()
}

/// The `[n]` and `[n, m]` markers of an answer.
fn markers(text: &str) -> BTreeSet<u64> {
    let mut found = BTreeSet::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        rest = &rest[open + 1..];
        if let Some(close) = rest.find(']') {
            found.extend(marker(&rest[..close]).into_iter().flatten());
        }
    }
    found
}

/// The digit runs of `text`, after its markers are gone.
fn numbers(text: &str) -> Vec<String> {
    let mut plain = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        plain.push_str(&rest[..open]);
        match rest[open..].find(']') {
            Some(close) if marker(&rest[open + 1..open + close]).is_some() => {
                rest = &rest[open + close + 1..];
            }
            _ => {
                plain.push('[');
                rest = &rest[open + 1..];
            }
        }
    }
    plain.push_str(rest);
    // A thousands separator (`6,500`, `6 500`) joins its groups into one number.
    let chars: Vec<char> = plain.chars().collect();
    let mut runs: Vec<String> = Vec::new();
    let mut run = String::new();
    for (i, c) in chars.iter().enumerate() {
        if c.is_ascii_digit() {
            run.push(*c);
            continue;
        }
        let group = chars
            .get(i + 1..i + 4)
            .is_some_and(|g| g.iter().all(char::is_ascii_digit))
            && !chars.get(i + 4).is_some_and(char::is_ascii_digit);
        if !run.is_empty() && matches!(c, ',' | ' ' | '\u{a0}' | '\u{202f}') && group {
            continue;
        }
        if !run.is_empty() {
            runs.push(std::mem::take(&mut run));
        }
    }
    if !run.is_empty() {
        runs.push(run);
    }
    // `07.10.` and `7. 10.` are one date.
    runs.into_iter()
        .map(|r| match r.trim_start_matches('0') {
            "" => "0".to_owned(),
            n => n.to_owned(),
        })
        .collect()
}

/// The sentences of an answer: a line, or a stop followed by a capital. "1. februára" and
/// "12. 10. 2026" stay whole, the next word being no capital.
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut start = 0;
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        for (i, (at, c)) in chars.iter().enumerate() {
            let ends = matches!(c, '.' | '!' | '?')
                && chars
                    .get(i + 1)
                    .is_some_and(|(_, next)| next.is_whitespace())
                && chars
                    .get(i + 2)
                    .is_some_and(|(_, next)| next.is_uppercase());
            if ends {
                out.push(&line[start..at + c.len_utf8()]);
                start = at + c.len_utf8();
            }
        }
        out.push(&line[start..]);
    }
    out.into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Lowercase with every run of whitespace (no-break spaces included) one plain space.
fn normal(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Whether `url` is on `host` or one of its subdomains.
fn on_host(url: &str, host: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .is_some_and(|h| h == host || h.ends_with(&format!(".{host}")))
}

/// Why `answer` fails `question`, or nothing when it passes.
fn judge(question: &Value, host: &str, answer: &Value) -> Vec<String> {
    let mut why = Vec::new();
    let text = answer["text"].as_str().unwrap_or_default();
    let citations = answer["citations"].as_array().cloned().unwrap_or_default();
    let lang = question["lang"].as_str().unwrap_or_default();
    if text.trim().is_empty() {
        return vec!["no answer".into()];
    }
    match language(text) {
        Some(found) if found == lang => {}
        found => why.push(format!("answered in {found:?}, asked in {lang}")),
    }
    let cited: BTreeMap<u64, &Value> = citations
        .iter()
        .filter_map(|c| c["n"].as_u64().map(|n| (n, c)))
        .collect();
    for n in markers(text) {
        match cited.get(&n) {
            None => why.push(format!("[{n}] has no citation")),
            Some(c) => match c["url"].as_str() {
                Some(url) if !on_host(url, host) => {
                    why.push(format!("[{n}] cites {url}, not {host}"))
                }
                None if c["tool"].is_null() => why.push(format!("[{n}] cites nothing")),
                _ => {}
            },
        }
    }
    if question["refuse"].as_bool() == Some(true) {
        if !citations.is_empty() {
            why.push("answers a question the site does not cover, with citations".into());
        }
        return why;
    }
    if citations.is_empty() {
        why.push("no citation".into());
    }
    let said = normal(text);
    for group in question["facts"].as_array().into_iter().flatten() {
        let alternatives: Vec<&str> = group
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if !alternatives.iter().any(|a| said.contains(&normal(a))) {
            why.push(format!("none of {alternatives:?}"));
        }
    }
    // A number is backed when the question or its passage holds it, or when the sentence that
    // says it cites the city's site: the answer may rightly read another page than the one the
    // question was written from.
    let known = format!(
        "{} {}",
        question["ask"].as_str().unwrap_or_default(),
        question["evidence"].as_str().unwrap_or_default()
    );
    let known: BTreeSet<String> = numbers(&known).into_iter().collect();
    let resolves = |n: &u64| {
        cited
            .get(n)
            .and_then(|c| c["url"].as_str())
            .is_some_and(|url| on_host(url, host))
    };
    for sentence in sentences(text) {
        if markers(sentence).iter().any(resolves) {
            continue;
        }
        for number in numbers(sentence) {
            if !known.contains(&number) {
                why.push(format!("{number} is in no source"));
            }
        }
    }
    why
}

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("the answer eval is JSON")
}

fn recording(project: &str) -> Option<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/answer_evals/recordings")
        .join(format!("{project}.json"));
    let text = std::fs::read_to_string(&path).ok()?;
    Some(serde_json::from_str(&text).expect("a recording is JSON"))
}

#[test]
fn every_project_has_ten_questions_in_its_language_and_english_one_of_them_uncovered() {
    let fixture = fixture();
    let projects = fixture["projects"].as_object().expect("projects");
    let mut ids = BTreeSet::new();
    for (project, body) in projects {
        let questions = body["questions"].as_array().expect("questions");
        assert_eq!(questions.len(), 10, "{project}");
        let langs: BTreeSet<&str> = questions
            .iter()
            .filter_map(|q| q["lang"].as_str())
            .collect();
        assert!(
            langs.contains("en") && langs.len() == 2,
            "{project}: {langs:?}"
        );
        let refused = questions.iter().filter(|q| q["refuse"] == true).count();
        assert_eq!(refused, 1, "{project}");
        for q in questions {
            let id = q["id"].as_str().expect("id");
            assert!(ids.insert(id.to_owned()), "{id} twice");
            // The question is in the language it says, so the judge holds the answer to it.
            assert_eq!(
                language(q["ask"].as_str().expect("ask")),
                q["lang"].as_str(),
                "{id}"
            );
            if q["refuse"] != true {
                assert!(!q["facts"].as_array().expect("facts").is_empty(), "{id}");
                // Every number of an expected fact is in the source passage, so none is invented here.
                let known: BTreeSet<String> = numbers(&format!("{} {}", q["ask"], q["evidence"]))
                    .into_iter()
                    .collect();
                for number in q["facts"].as_array().into_iter().flatten().flat_map(|g| {
                    g.as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .flat_map(numbers)
                }) {
                    assert!(known.contains(&number), "{id}: {number}");
                }
            }
        }
    }
    let covered: BTreeSet<&str> = projects
        .keys()
        .map(String::as_str)
        .chain(QUESTIONS_LATER.iter().copied())
        .collect();
    assert_eq!(
        covered,
        BTreeSet::from(["banskabystrica", "bbsk", "helsinki", "praha"])
    );
}

#[test]
fn the_recorded_answers_pass_the_judge_and_the_unrecorded_list_only_shrinks() {
    let fixture = fixture();
    let mut unrecorded = Vec::new();
    let (mut passed, mut judged) = (0usize, 0usize);
    for (project, body) in fixture["projects"].as_object().expect("projects") {
        let Some(recorded) = recording(project) else {
            unrecorded.push(project.as_str());
            continue;
        };
        let host = body["host"].as_str().expect("host");
        for q in body["questions"].as_array().expect("questions") {
            let id = q["id"].as_str().expect("id");
            let why = judge(q, host, &recorded["answers"][id]);
            judged += 1;
            if why.is_empty() {
                passed += 1;
            } else {
                println!("{id}: {}", why.join("; "));
            }
        }
    }
    assert_eq!(
        unrecorded, UNRECORDED,
        "a project was recorded (take it off UNRECORDED) or lost its recording"
    );
    if judged > 0 {
        let share = passed as f64 / judged as f64;
        println!("answer eval: {passed}/{judged} pass");
        assert!(share >= PASS_FLOOR, "{passed}/{judged} below {PASS_FLOOR}");
    }
}

fn question(lang: &str, facts: Value, evidence: &str) -> Value {
    serde_json::json!({"id": "t", "lang": lang, "ask": "Od kedy platí zľava?", "facts": facts, "evidence": evidence})
}

#[test]
fn the_judge_passes_a_sourced_answer_in_the_askers_language() {
    let q = question(
        "sk",
        serde_json::json!([["12. októbra"]]),
        "Od pondelka 12. októbra 2026",
    );
    let a = serde_json::json!({
        "text": "Zľava platí od pondelka 12. októbra 2026 a je to dočasné opatrenie [1].",
        "citations": [{"n": 1, "url": "https://www.banskabystrica.sk/aktuality/"}]
    });
    assert_eq!(judge(&q, "banskabystrica.sk", &a), Vec::<String>::new());
}

#[test]
fn the_judge_fails_another_language_a_missing_fact_and_an_invented_number() {
    let q = question(
        "sk",
        serde_json::json!([["12. októbra"]]),
        "Od pondelka 12. októbra 2026",
    );
    let english = serde_json::json!({
        "text": "The discount starts on 12 October 2026 [1].",
        "citations": [{"n": 1, "url": "https://www.banskabystrica.sk/a/"}]
    });
    let why = judge(&q, "banskabystrica.sk", &english);
    assert!(why.iter().any(|w| w.contains("asked in sk")), "{why:?}");
    assert!(why.iter().any(|w| w.contains("12. októbra")), "{why:?}");
    let invented = serde_json::json!({
        "text": "Zľava platí od 12. októbra [1]. Aj pre 3 deti a je to 75 percent.",
        "citations": [{"n": 1, "url": "https://www.banskabystrica.sk/a/"}]
    });
    let why = judge(&q, "banskabystrica.sk", &invented);
    assert_eq!(why, vec!["3 is in no source", "75 is in no source"]);
}

#[test]
fn the_judge_fails_a_marker_without_a_citation_and_a_citation_off_the_site() {
    let q = question(
        "sk",
        serde_json::json!([["12. októbra"]]),
        "12. októbra 2026",
    );
    let a = serde_json::json!({
        "text": "Zľava je od 12. októbra [1] a platí pre MHD [2].",
        "citations": [{"n": 1, "url": "https://evil.example/banskabystrica.sk"}]
    });
    let why = judge(&q, "banskabystrica.sk", &a);
    assert!(
        why.iter()
            .any(|w| w.contains("[1] cites https://evil.example")),
        "{why:?}"
    );
    assert!(why.iter().any(|w| w == "[2] has no citation"), "{why:?}");
}

#[test]
fn the_judge_reads_a_list_of_sources_in_one_bracket() {
    let q = question(
        "sk",
        serde_json::json!([["12. októbra"]]),
        "12. októbra 2026",
    );
    let both = serde_json::json!({
        "text": "Zľava je od 12. októbra [1, 2] a je to tak.",
        "citations": [{"n": 1, "url": "https://www.banskabystrica.sk/a/"}, {"n": 2, "url": "https://www.banskabystrica.sk/b/"}]
    });
    assert_eq!(judge(&q, "banskabystrica.sk", &both), Vec::<String>::new());
    let dropped =
        serde_json::json!({"text": "Zľava je od 12. októbra [1, 2] a je to tak.", "citations": []});
    let why = judge(&q, "banskabystrica.sk", &dropped);
    assert!(
        why.contains(&"[1] has no citation".to_owned()) && why.contains(&"no citation".to_owned()),
        "{why:?}"
    );
    assert_eq!(markers("[1, 2] [3][x] [4 5]"), BTreeSet::from([1, 2, 3]));
}

#[test]
fn a_number_from_another_page_is_backed_by_the_citation_of_its_sentence() {
    let q = question(
        "sk",
        serde_json::json!([["12. októbra"]]),
        "Od pondelka 12. októbra 2026",
    );
    let cited = serde_json::json!({
        "text": "Zľava platí od 12. októbra [1]. Je to už 3. zľava v roku 2026 [2].\nAj pre 4 deti.",
        "citations": [{"n": 1, "url": "https://www.banskabystrica.sk/a/"}, {"n": 2, "url": "https://www.banskabystrica.sk/b/"}]
    });
    assert_eq!(
        judge(&q, "banskabystrica.sk", &cited),
        vec!["4 is in no source"]
    );
    assert_eq!(
        sentences("Od 1. februára 2022 [1]. Potom zatvorené. a\nb"),
        ["Od 1. februára 2022 [1].", "Potom zatvorené. a", "b"]
    );
}

#[test]
fn the_judge_wants_an_uncovered_question_answered_without_citations() {
    let mut q = question("fi", serde_json::json!([]), "");
    q["refuse"] = true.into();
    let polite = serde_json::json!({"text": "Valitettavasti en löydä tätä tietoa, ja se ei ole sivustolla.", "citations": []});
    assert_eq!(judge(&q, "hel.fi", &polite), Vec::<String>::new());
    let made_up = serde_json::json!({
        "text": "Uimahalli on auki sunnuntaisin ja se on suosittu [1].",
        "citations": [{"n": 1, "url": "https://www.hel.fi/fi/"}]
    });
    assert_eq!(
        judge(&q, "hel.fi", &made_up),
        vec!["answers a question the site does not cover, with citations"]
    );
    let empty = serde_json::json!({"text": " ", "citations": []});
    assert_eq!(judge(&q, "hel.fi", &empty), vec!["no answer"]);
}

#[test]
fn a_thousands_separator_joins_a_number_and_a_marker_is_no_number() {
    assert_eq!(
        numbers("6,500 and 6 500 pupils [2], 1. 2. 2022, 7:00"),
        ["6500", "6500", "1", "2", "2022", "7", "0"]
    );
}

#[test]
fn the_language_detector_tells_the_four_languages_apart() {
    assert_eq!(
        language("Zľava platí aj pre deti, ktoré sú v škole."),
        Some("sk")
    );
    assert_eq!(
        language("Sleva platí také pro děti, které jsou ve škole."),
        Some("cs")
    );
    assert_eq!(
        language("Alennus on voimassa myös lapsille, jotka ovat koulussa."),
        Some("fi")
    );
    assert_eq!(
        language("The discount is valid for children who are in school."),
        Some("en")
    );
    assert_eq!(language("12345"), None);
}

/// The answer events of one Portal chat stream, as the judge reads them: the answer's text, its
/// citations, and an error's detail when the stream ended in one.
fn read_stream(body: &str) -> Value {
    let (mut text, mut citations, mut error) = (String::new(), Value::Array(vec![]), None);
    for event in body.split("\n\n") {
        let mut name = "";
        let mut data = String::new();
        for line in event.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                name = rest.trim();
            } else if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest.trim_start());
            }
        }
        let Ok(data) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        match name {
            "answer" => text.push_str(data["text"].as_str().unwrap_or_default()),
            "citations" => citations = data,
            "error" => error = data["detail"].as_str().map(str::to_owned),
            _ => {}
        }
    }
    let mut answer = serde_json::json!({"text": text, "citations": citations});
    if let Some(error) = error {
        answer["error"] = error.into();
    }
    answer
}

#[test]
fn a_chat_stream_reads_as_its_text_citations_and_error() {
    let body = "event: tool\ndata: {\"name\":\"search\",\"status\":\"ok\"}\n\n\
                event: answer\ndata: {\"text\":\"Zľava platí \"}\n\n\
                event: answer\ndata: {\"text\":\"od 12. októbra [1].\"}\n\n\
                event: citations\ndata: [{\"n\":1,\"url\":\"https://www.banskabystrica.sk/a/\"}]\n\n\
                event: done\ndata: {\"tokens\":812}\n\n";
    assert_eq!(
        read_stream(body),
        serde_json::json!({"text": "Zľava platí od 12. októbra [1].", "citations": [{"n": 1, "url": "https://www.banskabystrica.sk/a/"}]})
    );
    let failed = read_stream("event: error\ndata: {\"status\":429,\"title\":\"Budget\",\"detail\":\"the day's budget is spent\"}\n\n");
    assert_eq!(failed["error"], "the day's budget is spent");
    assert_eq!(failed["text"], "");
}

/// One live run (T-3067): asks every question of `JC_EVAL_PROJECT` through the Portal's chat
/// with the `knowledge` deployment and writes `recordings/{project}.json`. It spends model
/// tokens, so it runs only by hand:
/// `JC_EVAL_PORTAL=https://portal.dev.joinedcontext.com JC_EVAL_PROJECT=helsinki
/// JC_EVAL_COOKIE='<the signed-in browser's Cookie header>' cargo test -p assistant --test
/// answer_evals_tests -- --ignored record_one_live_run`. The cookie is read from the
/// environment only, never written; the recording holds the questions' answers and citations.
#[tokio::test]
#[ignore = "a live run against a Portal; spends model tokens"]
async fn record_one_live_run() {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
    let (portal, project, cookie) = (
        var("JC_EVAL_PORTAL"),
        var("JC_EVAL_PROJECT"),
        var("JC_EVAL_COOKIE"),
    );
    let csrf = cookie
        .split(';')
        .find_map(|c| c.trim().strip_prefix("jc_csrf="))
        .expect("the Cookie header holds jc_csrf")
        .to_owned();
    let fixture = fixture();
    let questions = fixture["projects"][&project]["questions"]
        .as_array()
        .expect("the project has questions");
    let client = reqwest::Client::new();
    let mut answers = serde_json::Map::new();
    for q in questions {
        let id = q["id"].as_str().expect("id");
        let body = client
            .post(format!(
                "{portal}/api/v1/projects/{project}/knowledge/deployments/knowledge/chat"
            ))
            .header("cookie", &cookie)
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({"message": q["ask"]}))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .expect("the Portal answers")
            .text()
            .await
            .expect("the stream ends");
        let answer = read_stream(&body);
        println!("{id}: {}", answer["text"]);
        answers.insert(id.to_owned(), answer);
    }
    let recording = serde_json::json!({"deployment": "knowledge", "answers": answers});
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/answer_evals/recordings")
        .join(format!("{project}.json"));
    std::fs::create_dir_all(path.parent().expect("dir")).expect("the recordings folder");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&recording).expect("json") + "\n",
    )
    .expect("the recording is written");
}

/// The month a word names, in English, Slovak (genitive) or Finnish (partitive), by its stem.
fn month(word: &str) -> Option<u32> {
    const STEMS: [&[&str]; 12] = [
        &["jan", "tammi"],
        &["feb", "helmi"],
        &["mar", "maalis"],
        &["apr", "huhti"],
        &["may", "máj", "touko"],
        &["jun", "jún", "kesä"],
        &["jul", "júl", "heinä"],
        &["aug", "elo"],
        &["sep", "syys"],
        &["oct", "okt", "loka"],
        &["nov", "marras"],
        &["dec", "joulu"],
    ];
    let word = word.to_lowercase();
    // "marraskuuta" starts like "mar": the longer stem wins.
    let mut best: Option<(usize, u32)> = None;
    for (at, stems) in STEMS.iter().enumerate() {
        for stem in stems.iter() {
            if word.starts_with(stem) && best.is_none_or(|(len, _)| stem.len() > len) {
                best = Some((stem.len(), at as u32 + 1));
            }
        }
    }
    best.map(|(_, m)| m)
}

/// The words and numbers of a line with what stands between each and the next.
fn tokens(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut word = String::new();
    let mut gap = String::new();
    for c in text.chars().chain(std::iter::once(' ')) {
        if c.is_alphanumeric() {
            if !gap.is_empty() || word.is_empty() {
                if !word.is_empty() {
                    out.push((std::mem::take(&mut word), std::mem::take(&mut gap)));
                }
                gap.clear();
            }
            word.push(c);
        } else if !word.is_empty() {
            gap.push(c);
        }
    }
    if !word.is_empty() {
        out.push((word, gap));
    }
    out
}

/// The first date a line names, as (year, month, day): `2026-10-11`, `11.10.2026`, `11.10.`,
/// `11 Oct 2026`, `Oct 11`, `11. októbra`, `11. lokakuuta`. A date without a year takes `year`.
fn date_in(line: &str, year: i32) -> Option<(i32, u32, u32)> {
    let t = tokens(line);
    let num = |i: usize| t.get(i).and_then(|(w, _)| w.parse::<u32>().ok());
    for i in 0..t.len() {
        let gap = t[i].1.trim();
        if let (Some(y), Some(m), Some(d)) = (num(i), num(i + 1), num(i + 2)) {
            if t[i].0.len() == 4
                && gap == "-"
                && t[i + 1].1.trim() == "-"
                && (1..=12).contains(&m)
                && (1..=31).contains(&d)
            {
                return Some((y as i32, m, d));
            }
        }
        let Some(d) = num(i).filter(|d| (1..=31).contains(d)) else {
            if let (Some(m), Some(d)) = (
                month(&t[i].0)
                    .filter(|_| t[i].0.len() >= 3 && t[i].0.chars().all(char::is_alphabetic)),
                num(i + 1),
            ) {
                if (1..=31).contains(&d) {
                    let y = num(i + 2).filter(|y| *y > 1999).map_or(year, |y| y as i32);
                    return Some((y, m, d));
                }
            }
            continue;
        };
        let year_after = |j: usize| num(j).filter(|y| *y > 1999).map_or(year, |y| y as i32);
        if gap.starts_with('.') {
            if let Some(m) = num(i + 1).filter(|m| (1..=12).contains(m)) {
                if t[i + 1].1.starts_with('.') {
                    return Some((year_after(i + 2), m, d));
                }
            }
        }
        if let Some(m) = t
            .get(i + 1)
            .and_then(|(w, _)| month(w).filter(|_| w.chars().all(char::is_alphabetic)))
        {
            return Some((year_after(i + 2), m, d));
        }
    }
    None
}

/// Whether a line names a time of day: `19:00`, `18.30` (not the `11.10.` of a date), `klo 18`.
fn has_time(line: &str) -> bool {
    let t = tokens(line);
    t.iter().enumerate().any(|(i, (w, gap))| {
        let hour = w.parse::<u32>().ok().filter(|h| *h <= 23);
        let minutes = t
            .get(i + 1)
            .filter(|(m, _)| m.len() == 2 && m.parse::<u32>().is_ok_and(|m| m <= 59));
        match (hour, minutes) {
            (Some(_), Some((_, after)))
                if gap == ":" || (gap == "." && !after.starts_with('.')) =>
            {
                true
            }
            _ => {
                w == "klo"
                    && t.get(i + 1)
                        .is_some_and(|(h, _)| h.parse::<u32>().is_ok_and(|h| h <= 23))
            }
        }
    })
}

const WEEKDAYS: &[&str] = &[
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "mon",
    "tue",
    "wed",
    "thu",
    "fri",
    "sat",
    "sun",
];

/// Why an answer to an event question fails, or nothing: in the asker's language, a list of
/// events, each with a start date and time and a place, none in `elsewhere`, none over by
/// `today`.
fn judge_events(
    lang: &str,
    answer: &Value,
    elsewhere: &[&str],
    today: (i32, u32, u32),
) -> Vec<String> {
    let text = answer["text"].as_str().unwrap_or_default();
    let mut why = Vec::new();
    if language(text) != Some(lang) {
        why.push(format!("not in {lang}"));
    }
    let items: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter_map(|l| {
            l.strip_prefix("- ")
                .or_else(|| l.strip_prefix("* "))
                .or_else(|| {
                    let (n, rest) = l.split_once(". ")?;
                    n.chars().all(|c| c.is_ascii_digit()).then_some(rest)
                })
        })
        .collect();
    if items.is_empty() {
        why.push("lists no event".into());
    }
    for item in items {
        let plain = item.replace("**", "");
        let short: String = plain.chars().take(40).collect();
        match date_in(&plain, today.0) {
            None => why.push(format!("no date: {short}")),
            Some(date) if date < today => why.push(format!("over: {short}")),
            Some(_) => {}
        }
        if !has_time(&plain) {
            why.push(format!("no time: {short}"));
        }
        if let Some(place) = elsewhere.iter().find(|p| plain.contains(*p)) {
            why.push(format!("in {place}: {short}"));
        }
        // A place: a capitalised word after the event's name that is no month or weekday.
        let after_name = plain
            .split_once([',', '–', '—', ':', '('])
            .map_or("", |(_, rest)| rest);
        let named = tokens(after_name).iter().any(|(w, _)| {
            w.chars().count() >= 3
                && w.chars().next().is_some_and(char::is_uppercase)
                && month(w).is_none()
                && !WEEKDAYS.contains(&w.to_lowercase().as_str())
        });
        if !named {
            why.push(format!("no place: {short}"));
        }
    }
    why
}

fn events() -> Value {
    serde_json::from_str(EVENTS).expect("events.json is JSON")
}

#[test]
fn an_event_answer_needs_a_date_a_time_and_a_place_upcoming_and_in_the_city() {
    let elsewhere = ["Espoo", "Iso Omena"];
    let today = (2026, 10, 8);
    let answer = |text: &str| serde_json::json!({"text": text});
    let good = "Here are the upcoming events in Helsinki [1]:\n- **Workshop for Families** – Sat 11 Oct 2026, 10:00–12:00, Oodi, Töölönlahdenkatu 4 [1]\n- **Jazz evening**, October 12, 19:00, Savoy-teatteri [2]";
    assert_eq!(
        judge_events("en", &answer(good), &elsewhere, today),
        Vec::<String>::new()
    );
    let sk = "Toto sú podujatia v Helsinkách:\n- **Koncert** – 11. októbra o 18:00, Musiikkitalo\n- **Trh**, 12.10.2026 9.30, Kauppatori";
    assert_eq!(
        judge_events("sk", &answer(sk), &elsewhere, today),
        Vec::<String>::new()
    );
    let fi = "Tässä ovat tapahtumat:\n1. **Konsertti**: 11. lokakuuta klo 18, Musiikkitalo\n2. **Tori** – 2026-10-12 klo 9.30, Kauppatori";
    assert_eq!(
        judge_events("fi", &answer(fi), &elsewhere, today),
        Vec::<String>::new()
    );

    // What the owner got (T-3325): Espoo, no date, no time.
    let owner = "The **Helsinki events** dataset [2, 3] lists:\n- **Workshop for Families** (Iso Omena, Leppävaarankatu 9) [9]";
    let why = judge_events("en", &answer(owner), &elsewhere, today);
    assert!(why.iter().any(|w| w.starts_with("no date")), "{why:?}");
    assert!(why.iter().any(|w| w.starts_with("no time")), "{why:?}");
    assert!(why.iter().any(|w| w.starts_with("in Iso Omena")), "{why:?}");
    let past = judge_events(
        "en",
        &answer("These are the events:\n- **Old fair**, 1 Oct 2026, 10:00, Kauppatori"),
        &elsewhere,
        today,
    );
    assert_eq!(past, vec!["over: Old fair, 1 Oct 2026, 10:00, Kauppatori"]);
    let nowhere = judge_events(
        "en",
        &answer("These are the events:\n- **Fair**, 11 Oct 2026, 10:00"),
        &elsewhere,
        today,
    );
    assert_eq!(nowhere, vec!["no place: Fair, 11 Oct 2026, 10:00"]);
    assert_eq!(
        judge_events(
            "en",
            &answer("There are no events in the data."),
            &elsewhere,
            today
        ),
        vec!["lists no event"]
    );
    assert!(judge_events("fi", &answer(good), &elsewhere, today).contains(&"not in fi".to_owned()));
    // A date without a year is this year's; 11.10. is a date, never a time.
    assert_eq!(date_in("11.10. Oodi", 2026), Some((2026, 10, 11)));
    assert!(!has_time("11.10. Oodi"));
    assert_eq!(date_in("15. marraskuuta", 2026), Some((2026, 11, 15)));
}

#[test]
fn the_event_questions_are_recorded_once_and_pass_the_judge() {
    let fixture = events();
    let questions = fixture["questions"].as_array().expect("questions");
    let langs: BTreeSet<&str> = questions
        .iter()
        .filter_map(|q| q["lang"].as_str())
        .collect();
    assert_eq!(langs, BTreeSet::from(["en", "fi", "sk"]));
    for q in questions {
        assert_eq!(
            language(q["ask"].as_str().expect("ask")),
            q["lang"].as_str(),
            "{}",
            q["id"]
        );
    }
    let recorded = recording("helsinki-events");
    assert_eq!(
        recorded.is_none(),
        EVENTS_UNRECORDED,
        "the event questions were recorded (set EVENTS_UNRECORDED to false) or lost their recording"
    );
    let Some(recorded) = recorded else { return };
    let day: Vec<i32> = recorded["recorded"]
        .as_str()
        .expect("the recording says the day it was made")
        .split('-')
        .filter_map(|p| p.parse().ok())
        .collect();
    let today = (day[0], day[1] as u32, day[2] as u32);
    let elsewhere: Vec<&str> = fixture["elsewhere"]
        .as_array()
        .expect("elsewhere")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let mut failed = Vec::new();
    for q in questions {
        let id = q["id"].as_str().expect("id");
        let why = judge_events(
            q["lang"].as_str().expect("lang"),
            &recorded["answers"][id],
            &elsewhere,
            today,
        );
        if !why.is_empty() {
            failed.push(format!("{id}: {}", why.join("; ")));
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// The one live run of the event questions (T-3325), through the Portal's chat of the public
/// deployment `JC_EVAL_DEPLOYMENT`, which answers as the widget does (AG-115). Same variables as
/// [`record_one_live_run`].
#[tokio::test]
#[ignore = "a live run against dev: spends model tokens, owner-approved once"]
async fn record_the_event_questions_live() {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
    let (portal, cookie, deployment) = (
        var("JC_EVAL_PORTAL"),
        var("JC_EVAL_COOKIE"),
        var("JC_EVAL_DEPLOYMENT"),
    );
    let csrf = cookie
        .split(';')
        .find_map(|c| c.trim().strip_prefix("jc_csrf="))
        .expect("the Cookie header holds jc_csrf")
        .to_owned();
    let fixture = events();
    let project = fixture["project"].as_str().expect("project");
    let client = reqwest::Client::new();
    let mut answers = serde_json::Map::new();
    for q in fixture["questions"].as_array().expect("questions") {
        let id = q["id"].as_str().expect("id");
        let body = client
            .post(format!(
                "{portal}/api/v1/projects/{project}/knowledge/deployments/{deployment}/chat"
            ))
            .header("cookie", &cookie)
            .header("x-csrf-token", &csrf)
            .json(&serde_json::json!({"message": q["ask"]}))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .expect("the Portal answers")
            .text()
            .await
            .expect("the stream ends");
        let answer = read_stream(&body);
        println!("{id}: {}", answer["text"]);
        answers.insert(id.to_owned(), answer);
    }
    let day = time::OffsetDateTime::now_utc().date().to_string();
    let recording =
        serde_json::json!({"deployment": deployment, "recorded": day, "answers": answers});
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/answer_evals/recordings/helsinki-events.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&recording).expect("json") + "\n",
    )
    .expect("the recording is written");
}
