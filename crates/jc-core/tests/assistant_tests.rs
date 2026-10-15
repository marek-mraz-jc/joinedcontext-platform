//! T-3051: `kind: KnowledgeSource` and `kind: AssistantDeployment` (MF-51, MF-52, ADR-N-040).
//!
//! Each refusal names the field a person changes, and a channel that answers people nobody
//! signed in never runs without explicit origins, a rate limit and a budget.

use jc_core::error::Error;
use jc_core::kinds::assistant::{SourceType, UploadLimits, DEFAULT_MODEL};
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
    // Only a website source lists WordPress types.
    let ckan = ckan.replace(
        "  wordpressTypes: [pages, newses, life_situations, regulations]\n",
        "",
    );
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

/// AG-118 (T-3226): a guide source reads the User Guide jc-assistant ships, so it names nothing to
/// fetch and no context space.
#[test]
fn a_guide_source_names_nothing_to_fetch() {
    let guide = |extra: &str| {
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: KnowledgeSource\n\
             metadata: {{ name: guide, namespace: helsinki }}\n\
             spec:\n  source: guide\n  visibility: public\n  languages: [en]\n{extra}"
        )
    };
    let read = source(&guide("")).expect("a guide source");
    assert_eq!(read.spec.source, SourceType::Guide);
    for (extra, field) in [
        ("  startUrls: [https://www.hel.fi/]\n", "source"),
        ("  ckanInstanceRef: hel-fi\n", "source"),
        ("  contextSpaces: [helsinki]\n", "source"),
    ] {
        assert_eq!(refused(source(&guide(extra))), field, "{extra}");
    }
}

/// T-4166 (MF-52): the public assistant's fields take safe defaults when a deployment names none.
#[test]
fn a_deployment_without_the_new_fields_takes_the_safe_defaults() {
    let spec = deployment(DEPLOYMENT)
        .expect("the documented deployment")
        .spec;
    assert!(spec.enabled);
    assert_eq!(spec.router_model(), DEFAULT_MODEL);
    assert_eq!(spec.answer_model(), DEFAULT_MODEL);
    assert!(spec.scope_projects().is_empty());
    assert!(spec.suggestions.is_empty() && spec.disclaimer.is_empty());
    let limit = spec.rate_limit.expect("rate limit");
    assert_eq!(
        (
            limit.per_client_per_hour,
            limit.per_client_per_day,
            limit.streams_per_client
        ),
        (60, 200, 2)
    );
    let budget = spec.budget.expect("budget");
    assert_eq!(budget.cost_per_day_usd, 3.0);
    assert_eq!(
        (
            budget.output_tokens_per_answer,
            budget.messages_per_conversation,
            budget.question_chars
        ),
        (700, 30, 1_000)
    );
}

/// The Banská Bystrica deployment as the rebuild declares it: every new field set, read back whole.
#[test]
fn a_deployment_with_every_new_field_validates_and_round_trips() {
    let yaml = deployment_with(
        "  sandbox: false\n",
        "  sandbox: false\n  enabled: false\n\
         \x20 model: { router: google/gemini-3.8-flash, answer: deepseek/deepseek-v4.1-flash }\n\
         \x20 suggestions:\n    sk: [\"Kedy je zber odpadu?\", \"Ako si vybavím parkovaciu kartu?\"]\n    en: [\"When is waste collected?\"]\n\
         \x20 disclaimer: { sk: \"Odpovede píše umelá inteligencia, overte si ich v zdroji.\" }\n\
         \x20 scope: { projects: [bbsk, mobilita] }\n",
    )
    .replace(
        "perClientPerMinute: 10 }",
        "perClientPerMinute: 10, perClientPerHour: 100, perClientPerDay: 300, streamsPerClient: 3 }",
    )
    .replace(
        "tokensPerConversation: 40000 }",
        "tokensPerConversation: 40000, costPerDayUsd: 2.5, outputTokensPerAnswer: 800, messagesPerConversation: 20, questionChars: 1500 }",
    );
    let parsed = deployment(&yaml).expect("every new field");
    assert!(!parsed.spec.enabled);
    assert_eq!(parsed.spec.answer_model(), "deepseek/deepseek-v4.1-flash");
    assert_eq!(parsed.spec.scope_projects(), ["bbsk", "mobilita"]);
    assert_eq!(parsed.spec.suggestions["sk"].len(), 2);
    assert_eq!(
        parsed.spec.budget.as_ref().map(|b| b.cost_per_day_usd),
        Some(2.5)
    );
    let again: AssistantDeployment =
        serde_norway::from_str(&serde_norway::to_string(&parsed).expect("yaml")).expect("parse");
    assert_eq!(again, parsed);
}

#[test]
fn a_new_deployment_field_out_of_bounds_is_refused_with_the_field_to_change() {
    let six = (0..6)
        .map(|i| format!("\"q{i}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let seven = (0..7)
        .map(|i| format!("\"q{i}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let long = "x".repeat(121);
    let disclaimer = "x".repeat(401);
    let ok =
        |extra: &str| deployment_with("  sandbox: false\n", &format!("  sandbox: false\n{extra}"));
    assert!(deployment(&ok(&format!("  suggestions: {{ sk: [{six}] }}\n"))).is_ok());
    assert!(deployment(&ok(&format!(
        "  disclaimer: {{ en: \"{}\" }}\n",
        "x".repeat(400)
    )))
    .is_ok());
    let cases = [
        ("  model: { router: Gemini }\n".to_owned(), "model.router"),
        (
            "  model: { answer: no-vendor }\n".to_owned(),
            "model.answer",
        ),
        (
            format!("  model: {{ answer: google/{} }}\n", "m".repeat(100)),
            "model.answer",
        ),
        ("  model: { answer: /flash }\n".to_owned(), "model.answer"),
        (
            format!("  suggestions: {{ sk: [{seven}] }}\n"),
            "suggestions",
        ),
        (
            format!("  suggestions: {{ sk: [\"{long}\"] }}\n"),
            "suggestions",
        ),
        (
            "  suggestions: { sk: [\"  \"] }\n".to_owned(),
            "suggestions",
        ),
        (
            "  suggestions: { de: [\"Wann?\"] }\n".to_owned(),
            "suggestions",
        ),
        (
            format!("  disclaimer: {{ sk: \"{disclaimer}\" }}\n"),
            "disclaimer",
        ),
        ("  disclaimer: { sk: \"\" }\n".to_owned(), "disclaimer"),
        (
            "  disclaimer: { cs: \"Odpovědi píše AI.\" }\n".to_owned(),
            "disclaimer",
        ),
        (
            "  scope: { projects: [\"*\"] }\n".to_owned(),
            "scope.projects",
        ),
        (
            "  scope: { projects: [Bad_Name] }\n".to_owned(),
            "scope.projects",
        ),
        (
            "  scope: { projects: [bbsk, bbsk] }\n".to_owned(),
            "scope.projects",
        ),
        (
            "  scope: { projects: [banskabystrica] }\n".to_owned(),
            "scope.projects",
        ),
        (
            format!(
                "  scope: {{ projects: [{}] }}\n",
                (0..11)
                    .map(|i| format!("p{i}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "scope.projects",
        ),
        ("  enabled: maybe\n".to_owned(), "parse"),
        ("  model: { router: a/b, apiKey: x }\n".to_owned(), "parse"),
        (
            "  scope: { projects: [a], organizations: [b] }\n".to_owned(),
            "parse",
        ),
    ];
    for (extra, field) in &cases {
        let got = refused(deployment(&ok(extra)));
        if *field == "parse" {
            assert!(
                got.contains("unknown field") || got.contains("invalid"),
                "{extra}: {got}"
            );
        } else {
            assert_eq!(got, *field, "{extra}");
        }
    }
    for (from, to, field) in [
        (
            "perClientPerMinute: 10 }",
            "perClientPerMinute: 10, perClientPerHour: 0 }",
            "rateLimit.perClientPerHour",
        ),
        (
            "perClientPerMinute: 10 }",
            "perClientPerMinute: 10, perClientPerDay: 100001 }",
            "rateLimit.perClientPerDay",
        ),
        (
            "perClientPerMinute: 10 }",
            "perClientPerMinute: 10, streamsPerClient: 0 }",
            "rateLimit.streamsPerClient",
        ),
        (
            "perClientPerMinute: 10 }",
            "perClientPerMinute: 10, streamsPerClient: 11 }",
            "rateLimit.streamsPerClient",
        ),
        (
            "tokensPerConversation: 40000 }",
            "tokensPerConversation: 40000, costPerDayUsd: 0 }",
            "budget.costPerDayUsd",
        ),
        (
            "tokensPerConversation: 40000 }",
            "tokensPerConversation: 40000, costPerDayUsd: 1000.5 }",
            "budget.costPerDayUsd",
        ),
        (
            "tokensPerConversation: 40000 }",
            "tokensPerConversation: 40000, costPerDayUsd: .nan }",
            "budget.costPerDayUsd",
        ),
        (
            "tokensPerConversation: 40000 }",
            "tokensPerConversation: 40000, outputTokensPerAnswer: 99 }",
            "budget.outputTokensPerAnswer",
        ),
        (
            "tokensPerConversation: 40000 }",
            "tokensPerConversation: 40000, outputTokensPerAnswer: 4001 }",
            "budget.outputTokensPerAnswer",
        ),
        (
            "tokensPerConversation: 40000 }",
            "tokensPerConversation: 40000, messagesPerConversation: 0 }",
            "budget.messagesPerConversation",
        ),
        (
            "tokensPerConversation: 40000 }",
            "tokensPerConversation: 40000, messagesPerConversation: 201 }",
            "budget.messagesPerConversation",
        ),
        (
            "tokensPerConversation: 40000 }",
            "tokensPerConversation: 40000, questionChars: 99 }",
            "budget.questionChars",
        ),
        (
            "tokensPerConversation: 40000 }",
            "tokensPerConversation: 40000, questionChars: 4001 }",
            "budget.questionChars",
        ),
    ] {
        assert_eq!(
            refused(deployment(&deployment_with(from, to))),
            field,
            "{to}"
        );
    }
}

/// A key pasted into the prompt, a suggestion or the disclaimer would reach every model call or
/// every visitor; the refusal names the field and never repeats the value.
#[test]
fn a_credential_in_the_texts_is_refused_without_repeating_it() {
    let token = "sk-or-v1-0123456789abcdef0123456789abcdef";
    for (from, to, field) in [
        (
            "systemPrompt: Odpovedaj stručne a vždy uveď zdroj.",
            format!("systemPrompt: Odpovedaj stručne, kľúč {token}"),
            "systemPrompt",
        ),
        (
            "  sandbox: false\n",
            format!("  sandbox: false\n  suggestions: {{ sk: [\"{token}\"] }}\n"),
            "suggestions",
        ),
        (
            "  sandbox: false\n",
            format!("  sandbox: false\n  disclaimer: {{ en: \"use {token} here\" }}\n"),
            "disclaimer",
        ),
    ] {
        let err = deployment(&deployment_with(from, &to)).expect_err("refused");
        assert!(
            !format!("{err:?} {err}").contains(token),
            "{field} repeated the value"
        );
        assert_eq!(refused(Err::<(), _>(err)), field);
    }
}

/// MF-51: a ckan source may narrow to its organizations; an upload source holds uploaded files.
#[test]
fn a_ckan_source_narrows_to_organizations_and_an_upload_source_names_nothing_to_fetch() {
    let ckan = |extra: &str| {
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: KnowledgeSource\n\
             metadata: {{ name: data, namespace: banskabystrica }}\n\
             spec:\n  source: ckan\n  ckanInstanceRef: data\n  visibility: public\n{extra}"
        )
    };
    let read = source(&ckan("  organizations: [banskabystrica, bbsk_kraj]\n")).expect("ckan");
    assert_eq!(read.spec.organizations, ["banskabystrica", "bbsk_kraj"]);
    assert!(source(&ckan(""))
        .expect("every organization")
        .spec
        .organizations
        .is_empty());
    let eleven = (0..11)
        .map(|i| format!("org{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    for (extra, field) in [
        (format!("  organizations: [{eleven}]\n"), "organizations"),
        (
            "  organizations: [Banska Bystrica]\n".to_owned(),
            "organizations",
        ),
        ("  organizations: [b]\n".to_owned(), "organizations"),
        ("  upload: { maxFiles: 5 }\n".to_owned(), "upload"),
    ] {
        assert_eq!(refused(source(&ckan(&extra))), field, "{extra}");
    }
    assert_eq!(
        refused(source(&source_with(
            "  visibility: public",
            "  visibility: public\n  organizations: [bb]"
        ))),
        "organizations"
    );

    let upload = |extra: &str| {
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: KnowledgeSource\n\
             metadata: {{ name: files, namespace: banskabystrica }}\n\
             spec:\n  source: upload\n  visibility: public\n  languages: [sk]\n{extra}"
        )
    };
    let read = source(&upload("")).expect("upload");
    assert_eq!(read.spec.source, SourceType::Upload);
    assert_eq!(
        read.spec.upload_limits(),
        UploadLimits {
            max_bytes: 20 * 1024 * 1024,
            max_files: 100
        }
    );
    let read =
        source(&upload("  upload: { maxBytes: 52428800, maxFiles: 500 }\n")).expect("the ceilings");
    assert_eq!(read.spec.upload_limits().max_files, 500);
    for (extra, field) in [
        ("  startUrls: [https://www.banskabystrica.sk/]\n", "source"),
        ("  ckanInstanceRef: data\n", "source"),
        ("  contextSpaces: [mesto]\n", "source"),
        ("  schedule: \"0 3 * * *\"\n", "schedule"),
        ("  upload: { maxBytes: 52428801 }\n", "upload.maxBytes"),
        ("  upload: { maxBytes: 0 }\n", "upload.maxBytes"),
        ("  upload: { maxFiles: 0 }\n", "upload.maxFiles"),
        ("  upload: { maxFiles: 501 }\n", "upload.maxFiles"),
    ] {
        assert_eq!(refused(source(&upload(extra))), field, "{extra}");
    }
    assert!(
        refused(source(&upload("  upload: { maxBytes: 1, types: [exe] }\n")))
            .contains("unknown field")
    );
}

/// MF-51, T-4154: a website source may list WordPress REST bases; nothing else may, and each is
/// a bounded REST base.
#[test]
fn a_website_source_lists_wordpress_types_and_no_other_source_does() {
    let read = source(SOURCE).expect("the documented source");
    assert_eq!(
        read.spec.wordpress_types,
        ["pages", "newses", "life_situations", "regulations"]
    );
    let with = |types: &str| {
        source_with(
            "  wordpressTypes: [pages, newses, life_situations, regulations]\n",
            &format!("  wordpressTypes: [{types}]\n"),
        )
    };
    assert!(source(&with("office_announcements, emergency-contacts, v2")).is_ok());
    let forty_one = (0..41)
        .map(|i| format!("t{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    for types in [
        forty_one.as_str(),
        "Pages",
        "pages/../users",
        "\"\"",
        "pages?per_page=100",
        &"a".repeat(65),
    ] {
        assert_eq!(refused(source(&with(types))), "wordpressTypes", "{types}");
    }
    let ckan = "apiVersion: joinedcontext.com/v1alpha1\nkind: KnowledgeSource\n\
                metadata: { name: data, namespace: banskabystrica }\n\
                spec:\n  source: ckan\n  ckanInstanceRef: data\n  wordpressTypes: [pages]\n";
    assert_eq!(refused(source(ckan)), "wordpressTypes");
}
