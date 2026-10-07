//! T-3051: `kind: KnowledgeSource` and `kind: AssistantDeployment` (MF-51, MF-52, ADR-N-040).
//!
//! Each refusal names the field a person changes, and a channel that answers people nobody
//! signed in never runs without explicit origins, a rate limit and a budget.

use jc_core::error::Error;
use jc_core::kinds::{AssistantDeployment, Channel, KnowledgeSource, PdfPolicyKind, Visibility};
use jc_core::registry;

const SOURCE: &str = include_str!("golden/028-KnowledgeSource-22-knowledge-assistant.yaml");
const DEPLOYMENT: &str = include_str!("golden/029-AssistantDeployment-22-knowledge-assistant.yaml");

fn source(yaml: &str) -> Result<KnowledgeSource, Error> {
    let parsed: KnowledgeSource =
        serde_norway::from_str(yaml).map_err(|e| Error::Parse(e.to_string()))?;
    parsed.validate()?;
    Ok(parsed)
}

fn deployment(yaml: &str) -> Result<AssistantDeployment, Error> {
    let parsed: AssistantDeployment =
        serde_norway::from_str(yaml).map_err(|e| Error::Parse(e.to_string()))?;
    parsed.validate()?;
    Ok(parsed)
}

/// The field a refusal names, or the parse error's text.
fn refused(result: Result<impl std::fmt::Debug, Error>) -> String {
    match result.expect_err("refused") {
        Error::Name { field, .. } => field.to_owned(),
        other => other.to_string(),
    }
}

/// `SOURCE`'s spec with `from` replaced by `to`, which must occur once.
fn source_with(from: &str, to: &str) -> String {
    assert_eq!(SOURCE.matches(from).count(), 1, "{from}");
    SOURCE.replace(from, to)
}

fn deployment_with(from: &str, to: &str) -> String {
    assert_eq!(DEPLOYMENT.matches(from).count(), 1, "{from}");
    DEPLOYMENT.replace(from, to)
}

#[test]
fn the_documented_examples_validate_and_round_trip() {
    let parsed = source(SOURCE).expect("the documented source");
    assert_eq!(parsed.spec.visibility, Visibility::Public);
    assert_eq!(parsed.spec.pdf.policy, PdfPolicyKind::Include);
    let again: KnowledgeSource =
        serde_norway::from_str(&serde_norway::to_string(&parsed).expect("yaml")).expect("parse");
    assert_eq!(again, parsed);

    let parsed = deployment(DEPLOYMENT).expect("the documented deployment");
    assert_eq!(parsed.spec.channel, Channel::Public);
    let again: AssistantDeployment =
        serde_norway::from_str(&serde_norway::to_string(&parsed).expect("yaml")).expect("parse");
    assert_eq!(again, parsed);

    for kind in ["KnowledgeSource", "AssistantDeployment"] {
        let info = registry::by_kind(kind).expect("catalogued");
        assert!(
            info.path_template
                .starts_with("projects/{project}/assistant/"),
            "{kind}"
        );
        assert!(registry::schema_of(kind).is_some(), "{kind}");
    }
    assert_eq!(
        registry::validate_yaml("KnowledgeSource", SOURCE).map(|r| r.is_ok()),
        Some(true)
    );
}

#[test]
fn a_minimal_source_takes_the_safe_defaults() {
    let parsed = source(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: KnowledgeSource\nmetadata: { name: s, namespace: p }\nspec:\n  source: website\n  startUrls: [https://example.org/]\n",
    )
    .expect("minimal");
    let spec = parsed.spec;
    assert_eq!(
        spec.visibility,
        Visibility::Internal,
        "a source is internal until somebody says otherwise"
    );
    assert!(!spec.off_domain_documents);
    assert!(spec.sitemap);
    assert_eq!((spec.max_depth, spec.max_pages), (3, 1_000));
    assert_eq!(
        (spec.pdf.max_bytes, spec.pdf.max_pages),
        (50 * 1024 * 1024, 500)
    );
}

#[test]
fn a_source_is_refused_with_the_field_to_change() {
    let cases = [
        (
            "startUrls: [https://www.banskabystrica.sk/]",
            "startUrls: [http://www.banskabystrica.sk/]",
            "startUrls",
        ),
        (
            "startUrls: [https://www.banskabystrica.sk/]",
            "startUrls: [\"https://\"]",
            "startUrls",
        ),
        (
            "startUrls: [https://www.banskabystrica.sk/]",
            "startUrls: []",
            "startUrls",
        ),
        (
            "startUrls: [https://www.banskabystrica.sk/]",
            "startUrls: [https://a.sk/]\n  ckanInstanceRef: open-data",
            "ckanInstanceRef",
        ),
        (
            "include: [\"/zivot-v-meste/**\", \"/samosprava/**\"]",
            "include: [\"zivot/**\"]",
            "include",
        ),
        ("maxDepth: 3", "maxDepth: 0", "maxDepth"),
        ("maxDepth: 3", "maxDepth: 11", "maxDepth"),
        ("maxPages: 2000", "maxPages: 50001", "maxPages"),
        ("maxBytes: 52428800", "maxBytes: 209715201", "pdf.maxBytes"),
        ("maxPages: 500 }", "maxPages: 2001 }", "pdf.maxPages"),
        (
            "schedule: \"0 3 * * *\"",
            "schedule: \"every night\"",
            "schedule",
        ),
        ("languages: [sk]", "languages: [slovak]", "languages"),
    ];
    for (from, to, field) in cases {
        assert_eq!(refused(source(&source_with(from, to))), field, "{to}");
    }
    let ckan = source_with("source: website               # website | ckan\n  startUrls: [https://www.banskabystrica.sk/]\n", "source: ckan\n");
    assert_eq!(
        refused(source(&ckan)),
        "ckanInstanceRef",
        "a ckan source names its instance"
    );
    let ckan = source_with("source: website               # website | ckan\n  startUrls: [https://www.banskabystrica.sk/]\n", "source: ckan\n  ckanInstanceRef: open-data\n");
    assert!(source(&ckan).is_ok());
    // An unknown field is a parse error, so a typo never becomes a silent default.
    assert!(refused(source(&source_with("maxDepth: 3", "maxDepht: 3"))).contains("unknown field"));
}

#[test]
fn an_origin_is_explicit_https_with_no_path_or_wildcard() {
    for origin in [
        "https://*.banskabystrica.sk",
        "*",
        "http://www.banskabystrica.sk",
        "https://www.banskabystrica.sk/",
        "https://www.banskabystrica.sk/chat",
        "https://user@www.banskabystrica.sk",
        "https://www.banskabystrica.sk:0",
        "https://-bad.sk",
    ] {
        let yaml = deployment_with(
            "allowedOrigins: [https://www.banskabystrica.sk]",
            &format!("allowedOrigins: [\"{origin}\"]"),
        );
        assert_eq!(refused(deployment(&yaml)), "allowedOrigins", "{origin}");
    }
    let yaml = deployment_with(
        "allowedOrigins: [https://www.banskabystrica.sk]",
        "allowedOrigins: [\"https://chat.banskabystrica.sk:8443\"]",
    );
    assert!(deployment(&yaml).is_ok());
}

#[test]
fn a_channel_nobody_signs_in_to_needs_origins_a_rate_limit_and_a_budget() {
    for (from, field) in [
        (
            "  allowedOrigins: [https://www.banskabystrica.sk]\n",
            "allowedOrigins",
        ),
        (
            "  rateLimit: { requestsPerMinute: 60, perClientPerMinute: 10 }\n",
            "rateLimit",
        ),
        (
            "  budget: { tokensPerDay: 2000000, tokensPerConversation: 40000 }\n",
            "budget",
        ),
    ] {
        for channel in ["public", "ckan", "iframe"] {
            let yaml = deployment_with(from, "")
                .replace("channel: public", &format!("channel: {channel}"));
            assert_eq!(
                refused(deployment(&yaml)),
                field,
                "{channel} without {field}"
            );
        }
        // The internal channel answers signed-in people, whose own rights and limits apply.
        let yaml = deployment_with(from, "").replace("channel: public", "channel: internal");
        assert!(deployment(&yaml).is_ok(), "internal without {field}");
    }
}

#[test]
fn a_deployment_is_refused_with_the_field_to_change() {
    let cases = [
        ("publicId: bb-public", "publicId: BB_Public", "publicId"),
        ("sources: [bb-web]", "sources: [Bad Name]", "sources"),
        ("tools: [query_entities]", "tools: []", "connectors[].tools"),
        (
            "timeoutSeconds: 20",
            "timeoutSeconds: 0",
            "connectors[].timeoutSeconds",
        ),
        (
            "timeoutSeconds: 20",
            "timeoutSeconds: 121",
            "connectors[].timeoutSeconds",
        ),
        (
            "requestsPerMinute: 60",
            "requestsPerMinute: 601",
            "rateLimit.requestsPerMinute",
        ),
        (
            "perClientPerMinute: 10",
            "perClientPerMinute: 61",
            "rateLimit.perClientPerMinute",
        ),
        (
            "tokensPerDay: 2000000",
            "tokensPerDay: 0",
            "budget.tokensPerDay",
        ),
        (
            "tokensPerConversation: 40000",
            "tokensPerConversation: 3000000",
            "budget.tokensPerConversation",
        ),
        (
            "primaryColor: \"#0b5394\"",
            "primaryColor: blue",
            "theme.primaryColor",
        ),
        ("languages: [sk, en]", "languages: [SK]", "languages"),
    ];
    for (from, to, field) in cases {
        assert_eq!(
            refused(deployment(&deployment_with(from, to))),
            field,
            "{to}"
        );
    }
    let nothing = deployment_with("  sources: [bb-web]\n", "").replace(
        "  connectors:\n    - { endpoint: mesto-verejne, tools: [query_entities], timeoutSeconds: 20 }\n",
        "",
    );
    assert_eq!(
        refused(deployment(&nothing)),
        "sources",
        "a deployment answers from something"
    );
    let long = deployment_with(
        "systemPrompt: Odpovedaj stručne a vždy uveď zdroj.",
        &format!("systemPrompt: {}", "a".repeat(8_001)),
    );
    assert_eq!(refused(deployment(&long)), "systemPrompt");
}

/// AG-116 (T-3225): a catalogue source reads the project's own Endpoints, so it names no start
/// URL and no CkanInstance, and only it names context spaces.
#[test]
fn a_catalogue_source_names_spaces_and_nothing_to_fetch() {
    let catalogue = |extra: &str| {
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: KnowledgeSource\n\
             metadata: {{ name: catalogue, namespace: banskabystrica }}\n\
             spec:\n  source: catalogue\n  visibility: public\n  languages: [sk, en]\n{extra}"
        )
    };
    let every = source(&catalogue("")).expect("every space");
    assert!(every.spec.context_spaces.is_empty());
    let some = source(&catalogue(
        "  contextSpaces: [ovzdusie, banskabystrica-verejne]\n",
    ))
    .expect("named spaces");
    assert_eq!(
        some.spec.context_spaces,
        ["ovzdusie", "banskabystrica-verejne"]
    );
    assert_eq!(
        refused(source(&catalogue(
            "  startUrls: [https://www.banskabystrica.sk/]\n"
        ))),
        "source"
    );
    assert_eq!(
        refused(source(&catalogue("  ckanInstanceRef: bb\n"))),
        "source"
    );
    assert_eq!(
        refused(source(&catalogue("  contextSpaces: [Not A Space]\n"))),
        "contextSpaces"
    );
    assert_eq!(
        refused(source(&source_with(
            "  visibility: public",
            "  visibility: public\n  contextSpaces: [ovzdusie]"
        ))),
        "contextSpaces"
    );
}
