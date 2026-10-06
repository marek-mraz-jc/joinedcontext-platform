//! T-3083, ADR-N-041 §3.4: a pipeline's `output.id` makes its ids by one last `mapping`
//! processor (`prefixed`, `keep`, `template`); without it the rendered file is the author's.

use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::{DataSourceSpec, IdMint, Mint, PipelineSpec};
use jcctl::bento::{id_processor, render, InputContext, RenderError};

const SOURCE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata: { name: weather, namespace: helsinki }
spec:
  type: http
  http: { url: "https://feed.example.org/weather.json", verb: GET, timeout: 15s }
"#;

const BENTO: &str = "pipeline:\n  processors:\n  - mapping: root = this\noutput:\n  stdout: {}\n";

fn source() -> DataSourceSpec {
    ResourceEnvelope::<DataSourceSpec>::from_yaml(SOURCE)
        .expect("parses")
        .spec
}

fn pipeline(id: &str) -> PipelineSpec {
    let yaml = format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Pipeline\nmetadata: {{ name: weather, namespace: helsinki }}\nspec:\n  class: resident\n  source: {{ dataSourceRef: {{ kind: DataSource, name: weather }} }}\n  targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:helsinki-all\n  output: {{ type: WeatherObserved, mode: upsert{id} }}\n"
    );
    ResourceEnvelope::<PipelineSpec>::from_yaml(&yaml)
        .expect("parses")
        .spec
}

fn rendered(spec: &PipelineSpec) -> Result<String, RenderError> {
    let context = InputContext::new("weather", "helsinki", "weather").with_pipeline_spec(spec);
    render(BENTO, &source(), &context)
}

/// The text of the last processor the render appended.
fn last_mapping(yaml: &str) -> String {
    let value: serde_norway::Value = serde_norway::from_str(yaml).expect("yaml");
    value["pipeline"]["processors"]
        .as_sequence()
        .and_then(|processors| processors.last())
        .and_then(|last| last["mapping"].as_str())
        .expect("a mapping processor")
        .to_owned()
}

#[test]
fn without_an_id_option_the_file_is_the_authors() {
    let plain = rendered(&pipeline("")).expect("renders");
    let none = render(
        BENTO,
        &source(),
        &InputContext::new("weather", "helsinki", "weather"),
    )
    .expect("renders");
    assert_eq!(plain, none, "an output without `id` adds nothing");
    assert_eq!(last_mapping(&plain), "root = this");
}

#[test]
fn prefixed_mints_the_platform_shape_from_a_field() {
    let yaml =
        rendered(&pipeline(", id: { mint: prefixed, from: station.code }")).expect("renders");
    assert_eq!(
        last_mapping(&yaml),
        "root = this\nroot.id = \"urn:ngsi-ld:WeatherObserved:%s:%s:%s\".format(env(\"JC_ORG_DOMAIN\"), env(\"JC_SPACE\"), this.station.code.string().re_replace_all(\"[^A-Za-z0-9._~-]\", \"-\"))\n"
    );
}

#[test]
fn keep_uses_the_sources_id_and_makes_a_urn_of_it_only_when_it_is_none() {
    let yaml = rendered(&pipeline(", id: { mint: keep, from: id }")).expect("renders");
    assert_eq!(
        last_mapping(&yaml),
        "root = this\nroot.id = if this.id.string().has_prefix(\"urn:ngsi-ld:\") { this.id.string() } else { \"urn:ngsi-ld:WeatherObserved:\" + this.id.string() }\n"
    );
}

#[test]
fn template_concatenates_its_text_and_fields() {
    let yaml = rendered(&pipeline(
        ", id: { mint: template, template: \"urn:ngsi-ld:WeatherObserved:{region}-{code}\" }",
    ))
    .expect("renders");
    assert_eq!(
        last_mapping(&yaml),
        "root = this\nroot.id = \"urn:ngsi-ld:WeatherObserved:\" + this.region.string() + \"-\" + this.code.string()\n"
    );
}

#[test]
fn nothing_a_manifest_writes_becomes_code() {
    let mint = |mint: Mint, from: Option<&str>, template: Option<&str>| IdMint {
        mint,
        from: from.map(str::to_owned),
        template: template.map(str::to_owned),
    };
    for (bad, why) in [
        (mint(Mint::Keep, None, None), "from"),
        (
            mint(Mint::Keep, Some("id) + env(\"JC_MODEL_KEY\""), None),
            "record path",
        ),
        (mint(Mint::Prefixed, Some("a..b"), None), "record path"),
        (
            mint(
                Mint::Template,
                None,
                Some("urn:ngsi-ld:WeatherObserved:${JC_MODEL_KEY}-{a}"),
            ),
            "RFC 8141",
        ),
        (
            mint(
                Mint::Template,
                None,
                Some("urn:ngsi-ld:WeatherObserved:\" + env(\"X\") + \"{a}"),
            ),
            "RFC 8141",
        ),
        (
            mint(Mint::Template, None, Some("urn:ngsi-ld:WeatherObserved:{a")),
            "closed",
        ),
        (
            mint(
                Mint::Template,
                None,
                Some("urn:ngsi-ld:WeatherObserved:{a b}"),
            ),
            "record path",
        ),
        (
            mint(Mint::Template, None, Some("urn:ngsi-ld:OtherType:{a}")),
            "type",
        ),
        (
            mint(
                Mint::Template,
                None,
                Some("urn:ngsi-ld:WeatherObserved:fixed"),
            ),
            "at least one",
        ),
        (
            mint(
                Mint::Template,
                Some("a"),
                Some("urn:ngsi-ld:WeatherObserved:{a}"),
            ),
            "`from` belongs",
        ),
    ] {
        let refused = id_processor(&bad, "WeatherObserved").expect_err(&format!("{bad:?}"));
        assert!(refused.to_string().contains(why), "{bad:?}: {refused}");
    }
    // The same checks stand at validation, so a proposal is refused before anything renders.
    let yaml = "apiVersion: joinedcontext.com/v1alpha1\nkind: Pipeline\nmetadata: { name: weather, namespace: helsinki }\nspec:\n  class: resident\n  source: { dataSourceRef: { kind: DataSource, name: weather } }\n  targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:helsinki-all\n  output: { type: WeatherObserved, mode: upsert, id: { mint: keep } }\n";
    let parsed = ResourceEnvelope::<PipelineSpec>::from_yaml(yaml).expect("parses");
    let refused = parsed
        .validate()
        .expect_err("keep without from is refused at validation");
    assert!(refused.to_string().contains("from"), "{refused}");
}
