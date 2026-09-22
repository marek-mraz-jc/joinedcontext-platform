//! Edge cases of `auth::dataspace_token::subject` (T-1896, EP-26, MP-02).
//!
//! Contract, in one sentence: a transfer token establishes a caller who is the agreement's
//! remote participant and **nothing else** — no user, no service account, no role, no group —
//! and only when the token names both an agreement and that participant, says when it was
//! issued, was minted to live no longer than `MAX_LIFETIME`, names an agreement this platform
//! is serving data under at `now`, names the participant that agreement was negotiated with,
//! and is presented to an endpoint the agreement's project reaches (DS-02, DS-11, DS-12).
//!
//! `dataspace_token_tests.rs` owns the happy path and one case per refusal. These are the ones
//! around them: which refusal wins when two apply, what the refusals look like to the caller,
//! the bound of the lifetime and the bound plus one, the near-misses of the participant DID,
//! and what a refusal carries into the answer.
//!
//! The one thing that cannot be tested here is the signature, the issuer and the audience:
//! `auth::token::verify` has checked them before `subject` is reached (T-1901), and a token
//! that failed there never gets this far.

use chrono::{DateTime, TimeZone, Utc};
use context_gateway::auth::dataspace_token::{
    agreements_of, presented, subject, Agreements, Refused, MAX_LIFETIME,
};
use context_gateway::auth::token::Claims;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use jcctl::loader::Repository;
use serde_json::{json, Value};

const SLUG: &str = "d7m2xq9vkt4zc6wrb8shj5nfp3";
const PROJECT: &str = "banskabystrica";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
const CONSUMER: &str = "did:web:helsinki.fi";
const AGREEMENT: &str = "urn:uuid:9a1f-air-quality";
const CONNECTOR: &str = "banskabystrica-dataspace-connector";
/// A fixed instant inside the fixture's validity window, so nothing here depends on the clock.
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
        .single()
        .expect("a real instant")
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jc-subject-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("a temporary directory");
    dir
}

/// The agreement as a manifest on disk, so the table is the one the gateway really builds.
fn written(participant: &str) -> Agreements {
    let dir = tempdir();
    let agreements = dir
        .join("projects")
        .join(PROJECT)
        .join("dataspace")
        .join("agreements");
    std::fs::create_dir_all(&agreements).expect("the repository directory");
    std::fs::write(
        agreements.join("air-quality.yaml"),
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\n\
             kind: DataAgreement\n\
             metadata:\n  name: air-quality\n  namespace: {PROJECT}\n\
             spec:\n  role: provider\n  offerRef: {{ kind: DataOffer, name: air-quality }}\n\
             \x20 remoteParticipant: {participant}\n\
             \x20 agreementId: \"{AGREEMENT}\"\n\
             \x20 state: finalized\n\
             \x20 validity: {{ from: 2026-01-01T00:00:00Z, to: 2027-01-01T00:00:00Z }}\n"
        ),
    )
    .expect("the agreement is written");
    agreements_of(&Repository::load(&dir).expect("the repository loads"))
}

fn serving_now() -> Agreements {
    written(CONSUMER)
}

fn policy() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: did, id: {CONSUMER} }}\n\
         operations: [queryEntity, retrieveEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n\
         \x20   propertyNames: [temperature]\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint() -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: SPACE.to_owned(),
        project: PROJECT.to_owned(),
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        policies: vec![policy()],
    }
}

/// The claims of a transfer token, with `lifetime` seconds between `iat` and `exp`.
fn transfer(lifetime: i64, participant: &str, agreement: &str) -> Value {
    json!({
        "iss": "https://id.example/realms/joinedcontext",
        "sub": "service-account-dataspace-connector",
        "aud": SLUG,
        "azp": CONNECTOR,
        "iat": now().timestamp(),
        "exp": now().timestamp() + lifetime,
        "agreementId": agreement,
        "participant": participant,
    })
}

fn claims_of(value: &Value) -> Claims {
    serde_json::from_value(value.clone()).expect("the claims parse")
}

fn establish(value: &Value) -> Result<context_gateway::pdp::evaluator::Subject, Refused> {
    subject(&claims_of(value), &serving_now(), &endpoint(), now())
}

#[test]
fn a_token_minted_for_exactly_the_longest_life_is_accepted_and_one_second_more_is_not() {
    assert!(
        establish(&transfer(MAX_LIFETIME, CONSUMER, AGREEMENT)).is_ok(),
        "the bound itself is inside DS-11",
    );
    assert_eq!(
        establish(&transfer(MAX_LIFETIME + 1, CONSUMER, AGREEMENT)),
        Err(Refused::TooLong),
        "the bound plus one second is not",
    );
    assert!(establish(&transfer(1, CONSUMER, AGREEMENT)).is_ok());
    assert!(establish(&transfer(0, CONSUMER, AGREEMENT)).is_ok());
}

/// DS-11 is a property of the token, so it is measured before anything is looked up: a token
/// minted to live a day is refused whether or not it names an agreement that exists, and the
/// refusal says nothing about which.
#[test]
fn the_lifetime_is_measured_before_the_agreement_is_looked_up() {
    assert_eq!(
        establish(&transfer(86_400, CONSUMER, "urn:uuid:nobody-declared-this")),
        Err(Refused::TooLong),
    );
    assert_eq!(
        establish(&transfer(86_400, "did:web:somebody.else", AGREEMENT)),
        Err(Refused::TooLong),
    );
}

/// A token whose `exp` precedes its `iat` has a negative lifetime, which is not a *long* one:
/// `subject` lets it by, and it is `auth::token::verify` that refuses an expired token
/// (T-1901). What is asserted here is that the arithmetic neither panics nor wraps into an
/// enormous lifetime that would be accepted by accident.
#[test]
fn a_lifetime_that_runs_backwards_neither_panics_nor_passes_for_a_long_one() {
    let mut claims = transfer(0, CONSUMER, AGREEMENT);
    claims["exp"] = json!(now().timestamp() - 86_400);
    assert!(
        establish(&claims).is_ok(),
        "expiry is the verifier's business"
    );

    let mut extreme = transfer(0, CONSUMER, AGREEMENT);
    extreme["iat"] = json!(i64::MAX);
    extreme["exp"] = json!(i64::MIN);
    assert!(
        establish(&extreme).is_ok(),
        "the ends of the range saturate rather than overflowing into a short lifetime",
    );

    let mut long = transfer(0, CONSUMER, AGREEMENT);
    long["iat"] = json!(i64::MIN);
    long["exp"] = json!(i64::MAX);
    assert_eq!(
        establish(&long),
        Err(Refused::TooLong),
        "the widest possible window is a long one, not a wrapped short one",
    );
}

#[test]
fn a_token_that_names_no_agreement_and_no_participant_establishes_nobody() {
    for (agreement, participant) in [
        (Some(""), Some(CONSUMER)),
        (Some(AGREEMENT), Some("")),
        (Some(""), Some("")),
        (None, Some(CONSUMER)),
        (Some(AGREEMENT), None),
        (None, None),
    ] {
        let mut claims = transfer(300, CONSUMER, AGREEMENT);
        let object = claims.as_object_mut().expect("an object");
        match agreement {
            Some(id) => {
                object.insert("agreementId".to_owned(), json!(id));
            }
            None => {
                object.remove("agreementId");
            }
        }
        match participant {
            Some(did) => {
                object.insert("participant".to_owned(), json!(did));
            }
            None => {
                object.remove("participant");
            }
        }
        assert_eq!(
            establish(&claims),
            Err(Refused::Incomplete),
            "agreement {agreement:?} with participant {participant:?} is not a transfer token",
        );
    }
}

/// `presented` decides whether these claims are checked as a transfer token at all. A token
/// with an empty `agreementId` is still presented as one — and is then refused — so an empty
/// claim can never fall through to the ServiceAccount that obtained the token (DS-02).
#[test]
fn an_empty_agreement_claim_is_still_a_transfer_token_and_is_refused_as_one() {
    let mut claims = transfer(300, CONSUMER, AGREEMENT);
    claims["agreementId"] = json!("");

    assert!(
        presented(&claims_of(&claims)),
        "it claims to act under an agreement"
    );
    assert_eq!(establish(&claims), Err(Refused::Incomplete));

    let mut none = transfer(300, CONSUMER, AGREEMENT);
    none.as_object_mut()
        .expect("an object")
        .remove("agreementId");
    assert!(
        !presented(&claims_of(&none)),
        "without the claim it is an ordinary token"
    );
}

#[test]
fn the_participant_did_is_matched_character_for_character() {
    assert!(establish(&transfer(300, CONSUMER, AGREEMENT)).is_ok());

    for near in [
        "DID:WEB:HELSINKI.FI",
        "did:web:helsinki.fi ",
        " did:web:helsinki.fi",
        "did:web:helsinki.fi/",
        "did:web:helsinki.fi\n",
        "did:web:helsinki.fi\0",
        "did%3Aweb%3Ahelsinki.fi",
        "did:web:helsinki.fi.evil.example",
        "did:web:helsinki.f",
        "did:web:helsinkiXfi",
    ] {
        assert_eq!(
            establish(&transfer(300, near, AGREEMENT)),
            Err(Refused::WrongParticipant(AGREEMENT.to_owned())),
            "{near:?} is not the participant the agreement names",
        );
    }
}

/// The id in a refusal is what the *token* said, and it never becomes a key into anything:
/// an agreement id carrying a newline or a null is refused like any other unknown id.
#[test]
fn an_agreement_id_that_is_not_one_is_refused_and_never_looked_up_loosely() {
    for hostile in [
        "urn:uuid:9a1f-air-quality\n",
        "urn:uuid:9a1f-air-quality\r\nX-Injected: 1",
        "urn:uuid:9a1f-air-quality\0",
        "URN:UUID:9A1F-AIR-QUALITY",
        "urn%3Auuid%3A9a1f-air-quality",
        "../../etc/passwd",
        "urn:uuid:*",
        " ",
    ] {
        assert_eq!(
            establish(&transfer(300, CONSUMER, hostile)),
            Err(Refused::NotServing(hostile.to_owned())),
            "{hostile:?} names no agreement this platform serves",
        );
    }
}

/// What the caller is told. Every refusal an outsider can reach is the same 401 with the same
/// fixed body, so a probe cannot tell an unknown agreement from a wrong participant from a
/// token minted too long (R20). `OutOfProject` is the one 403, and reaching it already needs a
/// token that names an agreement this platform serves and the participant it was negotiated
/// with — which is knowledge the caller had before it asked.
#[test]
fn every_refusal_tells_the_caller_the_same_thing() {
    let refusals = [
        Refused::Incomplete,
        Refused::NoLifetime,
        Refused::TooLong,
        Refused::NotServing(AGREEMENT.to_owned()),
        Refused::WrongParticipant(AGREEMENT.to_owned()),
    ];
    let answers: Vec<jc_core::ProblemDetails> = refusals.iter().cloned().map(Into::into).collect();
    for answer in &answers {
        assert_eq!(answer.status, 401);
        assert_eq!(answer.title, answers[0].title);
        assert_eq!(answer.detail, answers[0].detail);
        assert_eq!(answer.type_uri, answers[0].type_uri);
        assert!(answer.extensions.is_empty());
    }

    let out: jc_core::ProblemDetails = Refused::OutOfProject(AGREEMENT.to_owned()).into();
    assert_eq!(out.status, 403);
}

/// The agreement id travels inside the refusal so the gateway can log it; it must not reach
/// the caller, whose answer is the same fixed sentence whatever the token said.
#[test]
fn no_refusal_puts_the_agreement_id_or_a_newline_into_the_caller_s_answer() {
    let hostile = "urn:uuid:\r\nX-Injected: 1";
    for refused in [
        Refused::NotServing(hostile.to_owned()),
        Refused::WrongParticipant(hostile.to_owned()),
        Refused::OutOfProject(hostile.to_owned()),
    ] {
        let answer: jc_core::ProblemDetails = refused.into();
        let body = serde_json::to_string(&answer).expect("the answer serialises");
        assert!(!body.contains("urn:uuid:"), "the id is not in {body}");
        assert!(
            !body.contains("X-Injected"),
            "nor anything it carried: {body}"
        );
        assert!(
            !body.contains('\r') && !body.contains('\n'),
            "and no line break reaches a header or a body: {body}",
        );
    }
}

/// What the subject is, and what it is not. The connector obtains the token with its own
/// ServiceAccount, so `azp` names an account this repository declares with roles of its own —
/// and none of it may reach the subject, or a data space consumer would read as the connector
/// (DS-02).
#[test]
fn the_subject_is_the_participant_and_carries_nothing_of_the_connector() {
    let mut claims = transfer(300, CONSUMER, AGREEMENT);
    claims["realm_access"] = json!({ "roles": ["platform-admin", "space-writer"] });
    claims["groups"] = json!(["/administrators"]);
    claims["preferred_username"] = json!("connector-operator");

    let established = establish(&claims).expect("the token establishes the consumer");
    assert_eq!(established.did.as_deref(), Some(CONSUMER));
    assert_eq!(established.agreement.as_deref(), Some(AGREEMENT));
    assert_eq!(established.user, None);
    assert_eq!(established.service_account, None);
    assert!(
        established.roles.is_empty(),
        "a role in the token is the connector's, never the consumer's",
    );
    assert!(established.groups.is_empty());
}

/// The project binding is the agreement's, not the endpoint's: a public endpoint admits
/// everybody, so a transfer token works there, and a project-list endpoint admits only the
/// projects it names.
#[test]
fn which_endpoint_a_transfer_token_reaches_is_the_agreements_project_and_the_audience() {
    let claims = claims_of(&transfer(300, CONSUMER, AGREEMENT));

    let mut public = endpoint();
    public.audience = Audience::Public;
    public.project = "helsinki".to_owned();
    assert!(
        subject(&claims, &serving_now(), &public, now()).is_ok(),
        "a public endpoint admits any project, the agreement's included",
    );

    let mut listed = endpoint();
    listed.audience = Audience::ProjectList;
    listed.project = "helsinki".to_owned();
    listed.allowed_projects = vec![PROJECT.to_owned()];
    assert!(
        subject(&claims, &serving_now(), &listed, now()).is_ok(),
        "a project list that names the agreement's project admits it",
    );

    listed.allowed_projects = vec!["kosice".to_owned()];
    assert_eq!(
        subject(&claims, &serving_now(), &listed, now()),
        Err(Refused::OutOfProject(AGREEMENT.to_owned())),
        "one that does not, does not",
    );
}

/// The same token, twice, at the same instant: nothing about the first call changes the
/// second, and a refused call leaves nothing behind that a second call could pick up.
#[test]
fn the_same_token_twice_establishes_the_same_caller_and_a_refusal_leaves_nothing_behind() {
    let agreements = serving_now();
    let good = claims_of(&transfer(300, CONSUMER, AGREEMENT));
    let bad = claims_of(&transfer(300, "did:web:somebody.else", AGREEMENT));

    let first = subject(&good, &agreements, &endpoint(), now());
    let refused = subject(&bad, &agreements, &endpoint(), now());
    let second = subject(&good, &agreements, &endpoint(), now());

    assert!(refused.is_err());
    assert_eq!(first, second);
    assert!(first.is_ok());
}

/// The comparison is on the DID the manifest stores, as it was written: a stored participant
/// that merely looks like the token's — the consumer's domain with something after it — is a
/// different participant, and nothing normalises or prefix-matches either side.
#[test]
fn a_stored_participant_that_only_looks_like_the_tokens_did_does_not_match() {
    let padded = written("did:web:helsinki.fi.evil.example");
    assert_eq!(
        subject(
            &claims_of(&transfer(300, CONSUMER, AGREEMENT)),
            &padded,
            &endpoint(),
            now(),
        ),
        Err(Refused::WrongParticipant(AGREEMENT.to_owned())),
    );
}
