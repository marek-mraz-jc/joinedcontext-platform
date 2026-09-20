//! Edge cases of `auth::dataspace_token::Agreements::serving` (T-1895, EP-26, MP-02).
//!
//! Contract, in one sentence: `serving` hands back an agreement only when the id it is asked
//! for is that agreement's `spec.agreementId` character for character, this platform is the
//! **provider** in it, its state is `finalized`, and `now` is inside its validity window —
//! and for every other question it answers `None`, because the answer is what decides whether
//! a transfer token establishes anybody at all (DS-02, DS-11, DS-12).
//!
//! `dataspace_token_tests.rs` reads through the whole gateway and covers the happy path, a
//! terminated agreement, a window in the past and a consumer-role agreement. These are the
//! cases around those: the id compared against near-misses, both bounds of the window and the
//! second past each of them, the four states that are not `finalized`, and the table itself
//! empty or holding more than one agreement.
//!
//! The table cannot be built by hand — `Agreements` has no public constructor but
//! `agreements_of` — so every case here goes through a repository written to disk, which is
//! the table the gateway really serves from.

use chrono::{DateTime, TimeZone, Utc};
use context_gateway::auth::dataspace_token::{agreements_of, Agreements};
use jcctl::loader::Repository;

const PROJECT: &str = "banskabystrica";
const CONSUMER: &str = "did:web:helsinki.fi";
const AGREEMENT: &str = "urn:uuid:9a1f-air-quality";

fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, 12, 0, 0)
        .single()
        .expect("a real instant")
}

/// One agreement as a manifest writes it.
struct Written<'a> {
    name: &'a str,
    id: &'a str,
    role: &'a str,
    state: &'a str,
    validity: &'a str,
}

impl Default for Written<'_> {
    fn default() -> Self {
        Self {
            name: "air-quality",
            id: AGREEMENT,
            role: "provider",
            state: "finalized",
            validity: "{ from: 2026-01-01T00:00:00Z, to: 2027-01-01T00:00:00Z }",
        }
    }
}

/// A directory of this test's own; the operating system removes it, not a `Drop` nobody sees.
fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jc-serving-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("a temporary directory");
    dir
}

/// The agreements these manifests describe, loaded the way the gateway loads them.
fn table(written: &[Written<'_>]) -> Agreements {
    let dir = tempdir();
    let agreements = dir
        .join("projects")
        .join(PROJECT)
        .join("dataspace")
        .join("agreements");
    std::fs::create_dir_all(&agreements).expect("the repository directory");
    for one in written {
        let secret = if one.role == "consumer" {
            "  tokenSecretRef: { name: transfer-token, key: token }\n"
        } else {
            "  offerRef: { kind: DataOffer, name: air-quality }\n"
        };
        std::fs::write(
            agreements.join(format!("{}.yaml", one.name)),
            format!(
                "apiVersion: joinedcontext.com/v1alpha1\n\
                 kind: DataAgreement\n\
                 metadata:\n  name: {name}\n  namespace: {PROJECT}\n\
                 spec:\n  role: {role}\n{secret}\
                 \x20 remoteParticipant: {CONSUMER}\n\
                 \x20 agreementId: \"{id}\"\n\
                 \x20 state: {state}\n\
                 \x20 validity: {validity}\n",
                name = one.name,
                role = one.role,
                id = one.id,
                state = one.state,
                validity = one.validity,
            ),
        )
        .expect("the agreement is written");
    }
    agreements_of(&Repository::load(&dir).expect("the repository loads"))
}

/// The one agreement this platform serves data under right now.
fn serving_now() -> Agreements {
    table(&[Written::default()])
}

#[test]
fn the_agreement_id_is_matched_character_for_character() {
    let table = serving_now();
    assert!(table.serving(AGREEMENT, at(2026, 6, 1)).is_some());

    for near in [
        "",
        " ",
        "URN:UUID:9A1F-AIR-QUALITY",
        "urn:uuid:9a1f-air-qualit",
        "urn:uuid:9a1f-air-quality ",
        " urn:uuid:9a1f-air-quality",
        "urn:uuid:9a1f-air-quality/",
        "urn:uuid:9a1f-air-quality\n",
        "urn:uuid:9a1f-air-quality\0",
        "urn%3Auuid%3A9a1f-air-quality",
        "urn%253Auuid%253A9a1f-air-quality",
        "urn:uuid:9a1f-air-qualityy",
        "urn:uuid:9a1f-air-qualitʏ",
    ] {
        assert!(
            table.serving(near, at(2026, 6, 1)).is_none(),
            "{near:?} is not the agreement id, so it serves nothing",
        );
    }
}

#[test]
fn only_a_finalized_agreement_is_served() {
    for state in ["requested", "offered", "accepted", "terminated"] {
        let table = table(&[Written {
            state,
            ..Written::default()
        }]);
        assert!(
            table.serving(AGREEMENT, at(2026, 6, 1)).is_none(),
            "an agreement in state `{state}` serves nothing, whatever its window says",
        );
    }
    assert!(serving_now().serving(AGREEMENT, at(2026, 6, 1)).is_some());
}

/// DS-02: a consumer-role agreement is a token *this* platform holds to read somebody else's
/// endpoint. Honouring it here would read our own data under a credential we issued ourselves.
#[test]
fn a_consumer_role_agreement_is_never_served_however_live_it_is() {
    let table = table(&[Written {
        role: "consumer",
        ..Written::default()
    }]);
    assert!(table.serving(AGREEMENT, at(2026, 6, 1)).is_none());
    // And it is in the table: it is the role that refuses it, not a load that dropped it.
    assert_eq!(table.len(), 1);
}

#[test]
fn both_bounds_of_the_validity_window_are_inside_it() {
    let table = table(&[Written {
        validity: "{ from: 2026-01-01T00:00:00Z, to: 2026-12-31T23:59:59Z }",
        ..Written::default()
    }]);
    let instant = |text: &str| {
        DateTime::parse_from_rfc3339(text)
            .expect("an instant")
            .to_utc()
    };

    assert!(
        table
            .serving(AGREEMENT, instant("2026-01-01T00:00:00Z"))
            .is_some(),
        "the first second of the window is inside it",
    );
    assert!(
        table
            .serving(AGREEMENT, instant("2026-12-31T23:59:59Z"))
            .is_some(),
        "the last second of the window is inside it",
    );
    assert!(
        table
            .serving(AGREEMENT, instant("2025-12-31T23:59:59Z"))
            .is_none(),
        "the second before it is not",
    );
    assert!(
        table
            .serving(AGREEMENT, instant("2027-01-01T00:00:00Z"))
            .is_none(),
        "nor the second after it",
    );
}

#[test]
fn a_window_open_at_one_end_is_served_from_or_until_the_other() {
    let from_only = table(&[Written {
        validity: "{ from: 2026-01-01T00:00:00Z }",
        ..Written::default()
    }]);
    assert!(from_only.serving(AGREEMENT, at(2025, 12, 31)).is_none());
    assert!(from_only.serving(AGREEMENT, at(2030, 1, 1)).is_some());

    let to_only = table(&[Written {
        validity: "{ to: 2026-12-31T00:00:00Z }",
        ..Written::default()
    }]);
    assert!(to_only.serving(AGREEMENT, at(2020, 1, 1)).is_some());
    assert!(to_only.serving(AGREEMENT, at(2027, 1, 1)).is_none());
}

#[test]
fn an_agreement_with_no_window_at_all_is_served_at_any_instant() {
    let table = table(&[Written {
        validity: "{}",
        ..Written::default()
    }]);
    for now in [at(1971, 1, 1), at(2026, 6, 1), at(2999, 12, 31)] {
        assert!(table.serving(AGREEMENT, now).is_some(), "at {now}");
    }
}

/// A window of one instant: `from == to` is the narrowest one `validate` accepts, and it is
/// served at that instant and at no other.
#[test]
fn a_window_of_a_single_instant_is_served_at_that_instant_only() {
    let table = table(&[Written {
        validity: "{ from: 2026-06-01T12:00:00Z, to: 2026-06-01T12:00:00Z }",
        ..Written::default()
    }]);
    let instant = |text: &str| {
        DateTime::parse_from_rfc3339(text)
            .expect("an instant")
            .to_utc()
    };

    assert!(table
        .serving(AGREEMENT, instant("2026-06-01T12:00:00Z"))
        .is_some());
    assert!(table
        .serving(AGREEMENT, instant("2026-06-01T11:59:59Z"))
        .is_none());
    assert!(table
        .serving(AGREEMENT, instant("2026-06-01T12:00:01Z"))
        .is_none());
}

#[test]
fn a_gateway_that_knows_of_no_agreement_serves_nothing() {
    let none = Agreements::new();
    assert!(none.is_empty());
    assert!(none.serving(AGREEMENT, at(2026, 6, 1)).is_none());
    assert!(none.serving("", at(2026, 6, 1)).is_none());

    let loaded = table(&[]);
    assert!(loaded.is_empty());
    assert!(loaded.serving(AGREEMENT, at(2026, 6, 1)).is_none());
}

/// One agreement in the table never answers for another: the id is the whole of the lookup,
/// so a live agreement beside a terminated one does not make the terminated one serve.
#[test]
fn a_live_agreement_beside_a_dead_one_answers_only_for_itself() {
    let dead = "urn:uuid:0000-terminated";
    let table = table(&[
        Written::default(),
        Written {
            name: "terminated",
            id: dead,
            state: "terminated",
            ..Written::default()
        },
    ]);
    assert_eq!(table.len(), 2);

    assert!(table.serving(AGREEMENT, at(2026, 6, 1)).is_some());
    assert!(table.serving(dead, at(2026, 6, 1)).is_none());
}

/// The same question twice at the same instant is the same answer: `serving` reads a table
/// and holds no state of its own, so nothing about a first call changes a second.
#[test]
fn the_same_question_at_the_same_instant_is_the_same_answer() {
    let table = serving_now();
    let now = at(2026, 6, 1);
    let first = table.serving(AGREEMENT, now).cloned();
    let second = table.serving(AGREEMENT, now).cloned();
    assert_eq!(first, second);
    assert!(first.is_some());

    let refused = table.serving("urn:uuid:nothing", now);
    assert!(refused.is_none());
    assert!(table.serving("urn:uuid:nothing", now).is_none());
}

/// What comes back is the agreement itself, so the caller can read the project it binds the
/// token to — and that project is the manifest's namespace, not the file's name.
#[test]
fn what_is_served_carries_the_project_the_agreement_was_negotiated_in() {
    let table = serving_now();
    let served = table
        .serving(AGREEMENT, at(2026, 6, 1))
        .expect("it is served");

    assert_eq!(served.project, PROJECT);
    assert_eq!(served.spec.agreement_id, AGREEMENT);
    assert_eq!(served.spec.remote_participant.as_str(), CONSUMER);
}
