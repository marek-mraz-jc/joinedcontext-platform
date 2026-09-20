//! Edge cases of `auth::dataspace_token::agreements_of` (T-1897, EP-26, MP-02).
//!
//! Contract, in one sentence: the table holds one entry per `DataAgreement` the repository
//! declares whose spec both parses and validates, keyed by the agreement's **Dataspace
//! Protocol id** and carrying the project it was negotiated in — and an agreement the gateway
//! cannot read is left out whole rather than half-applied, so no token under it is honoured
//! (CC-08, DS-12).
//!
//! `dataspace_token_tests.rs` builds this table for every one of its cases, so the happy path
//! is well covered. These are the ones around it: what is left out and why, what the key
//! really is, where the project comes from, and what two agreements claiming the same id do.

use chrono::{DateTime, TimeZone, Utc};
use context_gateway::auth::dataspace_token::{agreements_of, Agreements};
use jcctl::loader::Repository;

const PROJECT: &str = "banskabystrica";
const CONSUMER: &str = "did:web:helsinki.fi";
const AGREEMENT: &str = "urn:uuid:9a1f-air-quality";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
        .single()
        .expect("a real instant")
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jc-agreements-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("a temporary directory");
    dir
}

/// One manifest, written where the repository loader looks for it.
struct File<'a> {
    project: &'a str,
    name: &'a str,
    body: &'a str,
}

/// A provider agreement as a manifest writes it, with everything the loader needs.
fn agreement_yaml(name: &str, project: &str, id: &str, extra: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\n\
         kind: DataAgreement\n\
         metadata:\n  name: {name}\n  namespace: {project}\n\
         spec:\n  role: provider\n  offerRef: {{ kind: DataOffer, name: air-quality }}\n\
         \x20 remoteParticipant: {CONSUMER}\n\
         \x20 agreementId: \"{id}\"\n\
         \x20 state: finalized\n\
         \x20 validity: {{ from: 2026-01-01T00:00:00Z, to: 2027-01-01T00:00:00Z }}\n{extra}"
    )
}

/// The table these files describe, loaded the way the gateway loads a repository.
fn table(files: &[File<'_>]) -> Agreements {
    let dir = tempdir();
    for file in files {
        let folder = dir
            .join("projects")
            .join(file.project)
            .join("dataspace")
            .join("agreements");
        std::fs::create_dir_all(&folder).expect("the repository directory");
        std::fs::write(folder.join(format!("{}.yaml", file.name)), file.body)
            .expect("the manifest is written");
    }
    agreements_of(&Repository::load(&dir).expect("the repository loads"))
}

fn one(name: &str, id: &str) -> String {
    agreement_yaml(name, PROJECT, id, "")
}

#[test]
fn a_repository_with_no_agreement_describes_a_gateway_that_knows_of_none() {
    let empty = table(&[]);
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    assert!(empty.serving(AGREEMENT, now()).is_none());
}

#[test]
fn the_key_is_the_agreement_id_and_never_the_manifests_own_name() {
    let table = table(&[File {
        project: PROJECT,
        name: "air-quality",
        body: &one("air-quality", AGREEMENT),
    }]);

    assert_eq!(table.len(), 1);
    assert!(table.serving(AGREEMENT, now()).is_some());
    assert!(
        table.serving("air-quality", now()).is_none(),
        "the file's name is not a Dataspace Protocol identifier and opens nothing",
    );
}

/// A spec the gateway cannot read is left out whole. `deny_unknown_fields` is what makes an
/// unknown member a parse failure, so a manifest written for a newer schema is not silently
/// served with the member ignored.
#[test]
fn an_agreement_whose_spec_does_not_parse_is_left_out_rather_than_half_applied() {
    for broken in [
        // A member no schema of this version declares.
        "  somethingElse: true\n",
        // A state that is not one of the five.
        "  state2: finalized\n",
    ] {
        let table = table(&[File {
            project: PROJECT,
            name: "air-quality",
            body: &agreement_yaml("air-quality", PROJECT, AGREEMENT, broken),
        }]);
        assert!(
            table.is_empty(),
            "a spec carrying {broken:?} is left out, so no token under it is honoured",
        );
    }
}

/// `validate` is run beside the parse, and an agreement that fails it is left out too: a
/// provider agreement with no `offerRef`, and a validity window that runs backwards.
#[test]
fn an_agreement_that_does_not_validate_is_left_out_too() {
    let no_offer = format!(
        "apiVersion: joinedcontext.com/v1alpha1\n\
         kind: DataAgreement\n\
         metadata:\n  name: air-quality\n  namespace: {PROJECT}\n\
         spec:\n  role: provider\n\
         \x20 remoteParticipant: {CONSUMER}\n\
         \x20 agreementId: \"{AGREEMENT}\"\n\
         \x20 state: finalized\n\
         \x20 validity: {{}}\n"
    );
    assert!(
        table(&[File {
            project: PROJECT,
            name: "air-quality",
            body: &no_offer
        }])
        .is_empty(),
        "a provider agreement names the offer it was negotiated from (DS-07)",
    );

    let backwards = agreement_yaml("air-quality", PROJECT, AGREEMENT, "").replace(
        "validity: { from: 2026-01-01T00:00:00Z, to: 2027-01-01T00:00:00Z }",
        "validity: { from: 2027-01-01T00:00:00Z, to: 2026-01-01T00:00:00Z }",
    );
    assert!(
        table(&[File {
            project: PROJECT,
            name: "air-quality",
            body: &backwards
        }])
        .is_empty(),
        "a window that ends before it starts is not a window",
    );

    let no_id = agreement_yaml("air-quality", PROJECT, "   ", "");
    assert!(
        table(&[File {
            project: PROJECT,
            name: "air-quality",
            body: &no_id
        }])
        .is_empty(),
        "an agreement id of nothing but spaces is no identifier (DS-09)",
    );
}

/// One unreadable agreement never takes a readable one down with it: the loop continues, and
/// the table is everything the gateway could read.
#[test]
fn one_unreadable_agreement_does_not_cost_the_others() {
    let other = "urn:uuid:0000-noise";
    let table = table(&[
        File {
            project: PROJECT,
            name: "air-quality",
            body: &one("air-quality", AGREEMENT),
        },
        File {
            project: PROJECT,
            name: "broken",
            body: &agreement_yaml("broken", PROJECT, "urn:uuid:broken", "  somethingElse: 1\n"),
        },
        File {
            project: PROJECT,
            name: "noise",
            body: &one("noise", other),
        },
    ]);

    assert_eq!(table.len(), 2);
    assert!(table.serving(AGREEMENT, now()).is_some());
    assert!(table.serving(other, now()).is_some());
    assert!(table.serving("urn:uuid:broken", now()).is_none());
}

/// Nothing but a `DataAgreement` reaches the table. The repository holds every kind, and a
/// `DataOffer` beside the agreement — whose spec has an `agreementId`-shaped member of its
/// own — must not become one.
#[test]
fn only_a_dataagreement_becomes_an_agreement() {
    let dir = tempdir();
    let folder = dir.join("projects").join(PROJECT).join("dataspace");
    std::fs::create_dir_all(folder.join("agreements")).expect("the repository directory");
    std::fs::create_dir_all(folder.join("offers")).expect("the repository directory");
    std::fs::write(
        folder.join("agreements").join("air-quality.yaml"),
        one("air-quality", AGREEMENT),
    )
    .expect("the agreement is written");
    std::fs::write(
        folder.join("offers").join("air-quality.yaml"),
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\n\
             kind: DataOffer\n\
             metadata:\n  name: air-quality\n  namespace: {PROJECT}\n\
             spec:\n  endpointRef: {{ kind: Endpoint, name: air-public }}\n\
             \x20 title: Air quality\n\
             \x20 policyRefs: []\n"
        ),
    )
    .expect("the offer is written");

    let table = agreements_of(&Repository::load(&dir).expect("the repository loads"));
    assert_eq!(table.len(), 1, "the offer is not an agreement");
    assert!(table.serving(AGREEMENT, now()).is_some());
}

/// The project is the manifest's namespace, because that is the project a token under this
/// agreement may be used in — `subject` refuses an endpoint the project does not reach.
#[test]
fn the_project_comes_from_the_namespace_the_manifest_declares() {
    let table = table(&[File {
        project: "kosice",
        name: "air-quality",
        body: &agreement_yaml("air-quality", "kosice", AGREEMENT, ""),
    }]);

    let served = table.serving(AGREEMENT, now()).expect("it is served");
    assert_eq!(served.project, "kosice");
}

/// Two agreements, two projects, the same Dataspace Protocol id: the gateway serves neither
/// (DS-09, DS-12, T-2355).
///
/// The identifier is unique by construction in DSP, so this is a repository somebody wrote wrong —
/// or wrote deliberately. Keeping either of them would decide, by nothing but the order the loader
/// walks in, which project's endpoints a transfer token under that id reaches: whoever may write a
/// manifest in any project could take another project's agreement id by choosing a namespace that
/// sorts after it, and with it the DID the id belongs to. Serving neither is the only outcome that
/// cannot be used that way, and `jcctl validate` names the collision so the repository is fixed
/// rather than left half-served (`loader_tests::two_agreements_claiming_one_id_are_a_finding`).
#[test]
fn two_projects_claiming_one_agreement_id_serve_neither() {
    let files = [
        File {
            project: "aaa",
            name: "air-quality",
            body: &agreement_yaml("air-quality", "aaa", AGREEMENT, ""),
        },
        File {
            project: "zzz",
            name: "air-quality",
            body: &agreement_yaml("air-quality", "zzz", AGREEMENT, ""),
        },
    ];

    let first = table(&files);
    let again = table(&files);
    assert!(
        first.serving(AGREEMENT, now()).is_none(),
        "a contested id serves nothing, whichever namespace sorts last",
    );
    assert!(first.is_empty(), "and neither claimant is in the table");
    assert_eq!(
        first.len(),
        again.len(),
        "the same repository builds the same table"
    );
}

/// The rule is about the id, not about the project boundary: two manifests of one project
/// claiming one id are contested in exactly the same way, and the file names do not decide it.
#[test]
fn one_project_claiming_an_id_twice_serves_neither() {
    let other = "urn:uuid:0000-noise";
    let table = table(&[
        File {
            project: PROJECT,
            name: "air-quality",
            body: &one("air-quality", AGREEMENT),
        },
        File {
            project: PROJECT,
            name: "air-quality-copy",
            body: &one("air-quality-copy", AGREEMENT),
        },
        File {
            project: PROJECT,
            name: "noise",
            body: &one("noise", other),
        },
    ]);

    assert!(table.serving(AGREEMENT, now()).is_none());
    assert!(
        table.serving(other, now()).is_some(),
        "an uncontested agreement beside them is served as before",
    );
    assert_eq!(table.len(), 1);
}

/// A manifest the gateway cannot read never contests an id: it is not in the table to begin with,
/// and letting it take a working agreement down would make a broken manifest in any project a way
/// to turn another project's transfer tokens off.
#[test]
fn an_unreadable_claimant_does_not_contest_the_id() {
    let table = table(&[
        File {
            project: "aaa",
            name: "broken",
            body: &agreement_yaml("broken", "aaa", AGREEMENT, "  somethingElse: true\n"),
        },
        File {
            project: "zzz",
            name: "air-quality",
            body: &agreement_yaml("air-quality", "zzz", AGREEMENT, ""),
        },
    ]);

    let served = table
        .serving(AGREEMENT, now())
        .expect("the one agreement that parses and validates is served");
    assert_eq!(served.project, "zzz");
}

/// The table is replaced whole by the reconcile that replaces the endpoint table, so the one
/// this function returns owes nothing to the one before it: an agreement removed from the
/// repository is gone from the next table, not merely inactive in it (DS-12, OPS-45).
#[test]
fn a_table_built_again_without_an_agreement_does_not_remember_it() {
    let with = table(&[File {
        project: PROJECT,
        name: "air-quality",
        body: &one("air-quality", AGREEMENT),
    }]);
    assert!(with.serving(AGREEMENT, now()).is_some());

    let without = table(&[]);
    assert!(without.serving(AGREEMENT, now()).is_none());
    assert!(without.is_empty());
}

/// An agreement id is a string the manifest author chose, and it is used as a map key and
/// nothing else: one carrying a newline, a null or a percent-encoding is stored and found
/// under exactly what was written, and opens nothing else.
#[test]
fn an_unusual_agreement_id_is_stored_and_found_under_exactly_what_was_written() {
    let odd = "urn:uuid:ωμέγα-air quality%2F..%2F";
    let table = table(&[File {
        project: PROJECT,
        name: "air-quality",
        body: &one("air-quality", odd),
    }]);

    assert_eq!(table.len(), 1);
    assert!(table.serving(odd, now()).is_some());
    for near in [
        "urn:uuid:ωμέγα-air quality/../",
        "urn:uuid:omega-air quality%2F..%2F",
        odd.trim_end(),
    ] {
        assert!(
            table.serving(near, now()).is_none() || near == odd,
            "{near:?} is not the id that was written",
        );
    }
}

/// A consumer-role agreement is loaded — the reaper and the connector both need to see it —
/// and `serving` is what refuses it. The two are separate on purpose: leaving it out of the
/// table would hide a terminated consumer agreement from everything that reports on it.
#[test]
fn a_consumer_agreement_is_in_the_table_and_serves_nothing() {
    let consumer = format!(
        "apiVersion: joinedcontext.com/v1alpha1\n\
         kind: DataAgreement\n\
         metadata:\n  name: air-quality\n  namespace: {PROJECT}\n\
         spec:\n  role: consumer\n\
         \x20 tokenSecretRef: {{ name: transfer-token, key: token }}\n\
         \x20 remoteParticipant: {CONSUMER}\n\
         \x20 agreementId: \"{AGREEMENT}\"\n\
         \x20 state: finalized\n\
         \x20 validity: {{ from: 2026-01-01T00:00:00Z, to: 2027-01-01T00:00:00Z }}\n"
    );
    let table = table(&[File {
        project: PROJECT,
        name: "air-quality",
        body: &consumer,
    }]);

    assert_eq!(table.len(), 1);
    assert!(table.serving(AGREEMENT, now()).is_none());
}
