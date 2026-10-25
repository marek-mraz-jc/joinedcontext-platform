//! The runner catalog knows which of its fields are credentials (T-2239; MF-24, MF-35, PL-50).
//!
//! `SECRET_FIELDS` decides two things: which field of a runner input the Data Sources form renders
//! as a reference widget, and which one `validate_runner` refuses unless it holds a `${VAR}`
//! naming an entry of `spec.secrets`. It is generated from the pinned runner's own `is_secret`
//! flag — and that flag is missing on fields that plainly carry a credential. Measured on dev
//! 2026-09-19: the `http_client` form offered a plain text box for `oauth.access_token`, so an
//! OAuth bearer typed there went into the manifest and into Git.
//!
//! The generator now appends an `ALSO_SECRET` list of its own. These cases are what that list is
//! for: they fail if a regeneration drops it, which is the way this hole would come back.
use jc_core::kinds::bento_inputs::SECRET_FIELDS;

/// Every field this platform calls a credential although the runner's documentation does not.
const ALSO_SECRET: &[(&str, &str)] = &[
    ("http_client", "oauth.access_token"),
    ("http_client", "digest_auth.password"),
    ("websocket", "oauth.access_token"),
    ("kafka", "sasl.access_token"),
    ("kafka", "sasl.aws.credentials.token"),
    ("kafka", "sasl.aws.credentials.id"),
    ("aws_s3", "credentials.token"),
    ("aws_s3", "credentials.id"),
    ("aws_sqs", "credentials.token"),
    ("aws_kinesis", "credentials.token"),
    ("sql_select", "credentials.token"),
    ("sql_select", "credentials.id"),
    ("sql_raw", "credentials.token"),
    ("pulsar", "auth.token.token"),
    ("twitter_search", "api_key"),
];

fn paths_of(input: &str) -> &'static [&'static str] {
    SECRET_FIELDS
        .iter()
        .find(|(name, _)| *name == input)
        .map(|(_, paths)| *paths)
        .unwrap_or_else(|| panic!("the catalog has no input named {input}"))
}

/// MF-24: a bearer token is a credential whether or not the runner's manual says so.
#[test]
fn every_credential_field_the_runner_does_not_flag_is_flagged_here() {
    for (input, path) in ALSO_SECRET {
        assert!(
            paths_of(input).contains(path),
            "{input}.{path} is not in SECRET_FIELDS: the form offers a box for its value and the \
             manifest keeps it (T-2239)"
        );
    }
}

/// What the runner does mark stays marked: the override adds and never replaces.
#[test]
fn the_runners_own_secret_fields_are_still_there() {
    for (input, path) in [
        ("http_client", "basic_auth.password"),
        ("http_client", "oauth.access_token_secret"),
        ("http_client", "tls.client_certs[].key"),
        ("aws_s3", "credentials.secret"),
        ("kafka", "sasl.password"),
        ("discord", "bot_token"),
    ] {
        assert!(
            paths_of(input).contains(&path),
            "{input}.{path} was dropped"
        );
    }
}

/// No path is listed twice: a duplicate would make the runner's rule report the same field twice.
#[test]
fn no_input_lists_a_secret_field_twice() {
    for (input, paths) in SECRET_FIELDS {
        let mut sorted = paths.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "{input} lists a field twice");
    }
}
