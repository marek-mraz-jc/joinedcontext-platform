//! The NGSI-LD query card (T-4759): the first data call of a question on an Endpoint comes back
//! with how to query it, its types and their attributes as compact lines with units and allowed
//! values, generated from the LinkML the Endpoint serves the caller. Never the LinkML itself: its
//! labels, prefixes and annotations cost ~1.9k tokens per Endpoint (median, prod), the card ~50.

use std::collections::{BTreeMap, HashSet};

use jc_core::display_order::{query_attributes, QueryAttribute};
use serde_json::Value;

use super::agent::Connected;

/// How the attribute lines read: in the cached rules, once, never per Endpoint.
pub const LEGEND: &str = "\n- After an Endpoint's first result comes its query card. Its attribute lines: `attr` text; `attr:range` a typed value; `attr:range[unit]` a number stored in that UCUM unit; `attr:a|b` the only allowed values; `attr:Type` a Relationship to an entity of that type; `attr:ref` a link value; `attr[]` several values.";

/// Above this many tokens a card lists its types with their attribute counts, and a type's lines
/// come with the first query on it (the BBSK catalogue: 24 types, 1,317 attributes, ~6.2k tokens).
pub const CARD_TOKENS: usize = 1_500;

/// Tokens of a text, about characters / 3.7 (the ratio of the board's measurements).
pub fn tokens(text: &str) -> usize {
    text.chars().count() * 10 / 37
}

/// The Endpoints and types the question's cards already gave, so each is given once.
#[derive(Default)]
pub struct Given {
    endpoints: HashSet<String>,
    /// The Endpoints whose card listed only the types, their lines still to come.
    brief: HashSet<String>,
    types: HashSet<(String, String)>,
}

impl Given {
    /// What the call's result starts with: the Endpoint's card on its first call of the question,
    /// and on a brief card's Endpoint the lines of a type the call queries first. `model` is the
    /// LinkML the Endpoint serves the caller; without it the card has the query rules alone.
    pub fn card(
        &mut self,
        connector: &Connected,
        tool: &str,
        arguments: &Value,
        model: Option<&str>,
    ) -> Option<String> {
        let endpoint = connector.endpoint.clone();
        let classes = model
            .and_then(|m| query_attributes(m).ok())
            .unwrap_or_default();
        let asked = queried(tool, arguments, &classes);
        let mut card = String::new();
        if self.endpoints.insert(endpoint.clone()) {
            let whole = card_of(connector, &classes, false);
            if tokens(&whole) <= CARD_TOKENS {
                for kind in classes.keys() {
                    self.types.insert((endpoint.clone(), kind.clone()));
                }
                card = whole;
            } else {
                self.brief.insert(endpoint.clone());
                card = card_of(connector, &classes, true);
            }
        }
        if self.brief.contains(&endpoint) {
            for kind in asked {
                if self.types.insert((endpoint.clone(), kind.clone())) {
                    card.push_str(&type_lines(&kind, &classes[&kind], &[]));
                }
            }
        }
        (!card.is_empty()).then_some(card)
    }
}

/// The model's classes a `query_entities` or `query_temporal` names in `type`.
fn queried(
    tool: &str,
    arguments: &Value,
    classes: &BTreeMap<String, Vec<QueryAttribute>>,
) -> Vec<String> {
    if !matches!(tool, "query_entities" | "query_temporal") {
        return Vec::new();
    }
    let named: Vec<&str> = match arguments.get("type") {
        Some(Value::String(one)) => vec![one],
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    named
        .into_iter()
        .map(|t| t.rsplit(['/', '#', ':']).next().unwrap_or(t))
        .filter(|t| classes.contains_key(*t))
        .map(str::to_owned)
        .collect()
}

/// The card: the fixed NGSI-LD lines, naming only the tools the Endpoint grants, then the
/// attributes every type shares once, then each type's lines, or with `brief` each type's count.
pub fn card_of(
    connector: &Connected,
    classes: &BTreeMap<String, Vec<QueryAttribute>>,
    brief: bool,
) -> String {
    let offers = |tool: &str| connector.tools.iter().any(|t| t.name == tool);
    let title = connector
        .title
        .as_deref()
        .map(|t| format!(" ({t})"))
        .unwrap_or_default();
    let reads: Vec<&str> = [
        ("query_entities", "query_entities (type, q, georel, pick)"),
        ("query_temporal", "query_temporal (history)"),
        ("summarize", "summarize (aggregates)"),
    ]
    .into_iter()
    .filter(|(tool, _)| offers(tool))
    .map(|(_, said)| said)
    .collect();
    let mut card = format!(
        "NGSI-LD Endpoint {}{title}, ETSI GS CIM 009 / TS 104 175.",
        connector.endpoint
    );
    if !reads.is_empty() {
        card.push_str(&format!(" Read it with {}.", reads.join(", ")));
    }
    if offers("query_entities") || offers("query_temporal") {
        card.push_str("\nq: attr==value; AND, | OR, == != > >= < <= ~= (regex), ranges a..b; a text value in quotes; a Relationship by its target id (refParentArea==\"urn:ngsi-ld:...\").");
    }
    if offers("query_entities") {
        card.push_str("\nGeo: georel near;maxDistance==<metres> or within on location. Units: compare in the unit shown (pm10 is ug/m3; a distance asked in km is sent in m).");
    }
    let shared = shared(classes);
    if !shared.is_empty() {
        card.push_str(&format!("\nEvery type also has: {}.", shared.join(", ")));
    }
    for (kind, attributes) in classes {
        if brief {
            card.push_str(&format!("\n{kind}: {} attributes", attributes.len()));
        } else {
            card.push_str(&type_lines(kind, attributes, &shared));
        }
    }
    if brief {
        card.push_str("\nA type's attributes come with your first query on it.");
    }
    card
}

/// The attribute lines every type writes alike, when there are two types or more.
fn shared(classes: &BTreeMap<String, Vec<QueryAttribute>>) -> Vec<String> {
    let mut all = classes.values();
    let (Some(first), true) = (all.next(), classes.len() > 1) else {
        return Vec::new();
    };
    first
        .iter()
        .map(|a| a.written.clone())
        .filter(|w| classes.values().all(|c| c.iter().any(|a| &a.written == w)))
        .collect()
}

/// One type's line, without the `shared` attributes, and its example `q` when it has one.
fn type_lines(kind: &str, attributes: &[QueryAttribute], shared: &[String]) -> String {
    let own: Vec<&str> = attributes
        .iter()
        .map(|a| a.written.as_str())
        .filter(|w| !shared.iter().any(|s| s == w))
        .collect();
    let mut lines = format!("\n{kind}: {}", own.join(", "));
    if let Some(example) = attributes.iter().find_map(|a| a.example.as_deref()) {
        lines.push_str(&format!("\n  e.g. q={example}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::super::mcp::{Surface, Tool};
    use super::*;

    const MODEL: &str = r#"
"enums":
  "Mode":
    "permissible_values": { "metro": {}, "tram": {} }
"slots":
  "name": {}
  "temperature": { "range": "float", "unit": { "ucum_code": "Cel" } }
  "pm10": { "range": "float", "unit": { "ucum_code": "ug/m3" } }
  "transportMode": { "range": "Mode" }
  "refParentArea":
    "range": "AdministrativeArea"
    "annotations": { "ngsi_ld_kind": { "tag": "ngsi_ld_kind", "value": "Relationship" } }
  "lineName": {}
"classes":
  "AirQualityObserved": { "slots": ["name", "pm10", "temperature", "refParentArea"] }
  "Route": { "slots": ["name", "transportMode", "lineName"] }
"#;

    fn connector(tools: &[&str]) -> Connected {
        Connected {
            endpoint: "praha/ovzdusi".into(),
            title: Some("Air in Prague".into()),
            surface: Surface {
                gateway: "http://gateway".into(),
                slug: "ovzdusi".into(),
                timeout: Duration::from_secs(1),
                token: None,
            },
            tools: tools
                .iter()
                .map(|name| Tool {
                    name: (*name).into(),
                    description: String::new(),
                    input_schema: json!({"type": "object"}),
                })
                .collect(),
            label: None,
            siblings: Vec::new(),
        }
    }

    #[test]
    fn the_card_says_ngsi_ld_units_values_relationships_and_only_the_granted_tools() {
        let classes = query_attributes(MODEL).unwrap();
        let card = card_of(&connector(&["query_entities", "get_type"]), &classes, false);
        assert!(
            card.starts_with("NGSI-LD Endpoint praha/ovzdusi (Air in Prague), ETSI GS CIM 009"),
            "{card}"
        );
        assert!(
            card.contains("Read it with query_entities (type, q, georel, pick)."),
            "{card}"
        );
        assert!(
            !card.contains("query_temporal") && !card.contains("summarize"),
            "{card}"
        );
        assert!(card.contains("\nEvery type also has: name."), "{card}");
        assert!(card.contains("\nAirQualityObserved: pm10:float[ug/m3], temperature:float[Cel], refParentArea:AdministrativeArea\n  e.g. q=pm10>0"), "{card}");
        assert!(
            card.contains(
                "\nRoute: transportMode:metro|tram, lineName\n  e.g. q=transportMode==\"metro\""
            ),
            "{card}"
        );
        // An attribute the served model does not hold, the caller may not read: never named.
        assert!(!card.contains("secret"));
        let temporal = card_of(&connector(&["query_temporal"]), &classes, false);
        assert!(
            temporal.contains("Read it with query_temporal (history).")
                && !temporal.contains("\nGeo:"),
            "{temporal}"
        );
        let bare = card_of(&connector(&["get_type"]), &BTreeMap::new(), false);
        assert_eq!(
            bare,
            "NGSI-LD Endpoint praha/ovzdusi (Air in Prague), ETSI GS CIM 009 / TS 104 175."
        );
    }

    #[test]
    fn each_example_is_a_q_the_gateway_parses_on_its_own_types_attributes() {
        let classes = query_attributes(MODEL).unwrap();
        for (kind, attributes) in &classes {
            for example in attributes.iter().filter_map(|a| a.example.as_deref()) {
                antares_ql::parse_q(example).unwrap_or_else(|e| panic!("{kind}: {example}: {e}"));
                let named = example.split(['>', '=']).next().unwrap();
                assert!(
                    attributes.iter().any(|a| a.slot == named),
                    "{kind}: {example}"
                );
            }
        }
    }

    #[test]
    fn a_card_comes_once_per_endpoint_and_a_brief_ones_type_lines_with_its_first_query() {
        let c = connector(&["query_entities"]);
        let mut given = Given::default();
        let first = given
            .card(&c, "list_types", &json!({}), Some(MODEL))
            .unwrap();
        assert!(first.contains("\nRoute: "));
        assert_eq!(
            given.card(&c, "query_entities", &json!({"type": "Route"}), Some(MODEL)),
            None
        );
        // A model past CARD_TOKENS: the types with their counts, then a type's lines on its query.
        let slots: String = (0..400)
            .map(|i| format!("  attribute{i}: {{ range: integer }}\n"))
            .collect();
        let names: Vec<String> = (0..400).map(|i| format!("attribute{i}")).collect();
        let big = format!("slots:\n{slots}classes:\n  Grant: {{ slots: [{}] }}\n  School: {{ slots: [attribute0] }}\n", names.join(", "));
        let mut given = Given::default();
        let brief = given
            .card(
                &c,
                "query_entities",
                &json!({"type": ["Grant"]}),
                Some(&big),
            )
            .unwrap();
        assert!(brief.contains("\nGrant: 400 attributes\nSchool: 1 attributes\nA type's attributes come with your first query on it.\nGrant: attribute0:integer, attribute1:integer"), "{}", &brief[..600]);
        assert!(tokens(&brief) > CARD_TOKENS);
        let school = given
            .card(
                &c,
                "query_entities",
                &json!({"type": "urn:x:School"}),
                Some(&big),
            )
            .unwrap();
        assert_eq!(
            school,
            "\nSchool: attribute0:integer\n  e.g. q=attribute0>0"
        );
        assert_eq!(
            given.card(&c, "query_temporal", &json!({"type": "School"}), Some(&big)),
            None
        );
        assert_eq!(
            given.card(&c, "get_type", &json!({"type": "Grant"}), Some(&big)),
            None
        );
        // No model: the rules alone, once.
        let mut given = Given::default();
        assert!(given
            .card(&c, "query_entities", &json!({}), None)
            .unwrap()
            .starts_with("NGSI-LD Endpoint"));
        assert_eq!(given.card(&c, "query_entities", &json!({}), None), None);
    }
}

#[cfg(test)]
mod edges {
    use std::time::Duration;

    use serde_json::json;

    use super::super::mcp::{Surface, Tool};
    use super::*;

    fn connector(endpoint: &str) -> Connected {
        Connected {
            endpoint: endpoint.into(),
            title: None,
            surface: Surface {
                gateway: "http://gateway".into(),
                slug: endpoint.into(),
                timeout: Duration::from_secs(1),
                token: None,
            },
            tools: vec![Tool {
                name: "query_entities".into(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
            }],
            label: None,
            siblings: Vec::new(),
        }
    }

    /// A model the card cannot read still gives the query rules; a type the served model does
    /// not hold (one the caller may not read) never gets lines; one card per Endpoint, each its own.
    #[test]
    fn a_broken_model_an_unserved_type_and_two_endpoints() {
        let mut given = Given::default();
        let a = connector("a/one");
        let broken = given.card(
            &a,
            "query_entities",
            &json!({"type": "School"}),
            Some("classes: ["),
        );
        assert_eq!(
            broken.as_deref(),
            Some("NGSI-LD Endpoint a/one, ETSI GS CIM 009 / TS 104 175. Read it with query_entities (type, q, georel, pick).\nq: attr==value; AND, | OR, == != > >= < <= ~= (regex), ranges a..b; a text value in quotes; a Relationship by its target id (refParentArea==\"urn:ngsi-ld:...\").\nGeo: georel near;maxDistance==<metres> or within on location. Units: compare in the unit shown (pm10 is ug/m3; a distance asked in km is sent in m).")
        );
        let model =
            "slots:\n  pupils: { range: integer }\nclasses:\n  School: { slots: [pupils] }\n";
        let b = connector("b/two");
        let card = given
            .card(
                &b,
                "query_entities",
                &json!({"type": ["Payroll"]}),
                Some(model),
            )
            .unwrap();
        assert!(
            !card.contains("Payroll") && card.contains("\nSchool: pupils:integer"),
            "{card}"
        );
        assert!(
            !card.contains("Every type also has"),
            "one type shares nothing: {card}"
        );
        assert_eq!(
            given.card(
                &b,
                "query_entities",
                &json!({"type": "Payroll"}),
                Some(model)
            ),
            None
        );
        assert_eq!(
            given.card(&a, "query_entities", &json!({}), Some(model)),
            None
        );
    }
}

#[cfg(test)]
mod sizes {
    use std::time::Duration;

    use serde_json::Value;

    use super::super::mcp::{Surface, Tool};
    use super::*;

    /// The public Endpoints' LinkML on the prod hub (2026-10-19 19:30Z, 87 that answered of 96),
    /// cut to what a card reads: classes, slots with range, unit, multivalued and NGSI-LD kind,
    /// and the enums' values. Labels, descriptions and prefixes, most of the weight, are gone.
    const SERVED: &str = include_str!("../../tests/fixtures/mcp/linkml-public-endpoints.json");

    fn connector(slug: &str) -> Connected {
        let tool = |name: &str| Tool {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        Connected {
            endpoint: format!("project/{slug}"),
            title: Some("A public dataset".into()),
            surface: Surface {
                gateway: "http://gateway".into(),
                slug: slug.into(),
                timeout: Duration::from_secs(1),
                token: None,
            },
            tools: ["query_entities", "query_temporal", "summarize"]
                .map(tool)
                .into(),
            label: None,
            siblings: Vec::new(),
        }
    }

    /// The card each Endpoint's first call carries, in tokens; byte-identical on a second run.
    fn cards() -> Vec<(String, usize, usize)> {
        let served: BTreeMap<String, Value> = serde_json::from_str(SERVED).unwrap();
        served
            .iter()
            .map(|(slug, model)| {
                let model = model.to_string();
                let c = connector(slug);
                let card = Given::default()
                    .card(&c, "list_types", &Value::Null, Some(&model))
                    .unwrap();
                let again = Given::default()
                    .card(&c, "list_types", &Value::Null, Some(&model))
                    .unwrap();
                assert_eq!(card, again, "{slug}");
                let classes = query_attributes(&model).unwrap();
                (
                    slug.clone(),
                    tokens(&card),
                    tokens(&card_of(&c, &classes, false)),
                )
            })
            .collect()
    }

    #[test]
    fn the_cards_of_the_public_endpoints_stay_small() {
        let cards = cards();
        let mut given: Vec<usize> = cards.iter().map(|(_, g, _)| *g).collect();
        given.sort_unstable();
        let whole: usize = cards.iter().map(|(_, _, w)| w).sum();
        let mean = given.iter().sum::<usize>() / given.len();
        let median = given[given.len() / 2];
        let max = *given.last().unwrap();
        // Measured 2026-10-19: 87 cards, mean 224, median 195, max 754 tokens; the LinkML they
        // replace: mean ~4.2k, median ~1.9k, max ~75k. About 140 of each card is the fixed query
        // header; the rest is the type lines.
        assert_eq!(given.len(), 87);
        assert!(mean <= 260 && median <= 230, "mean {mean}, median {median}");
        assert!(max <= CARD_TOKENS, "max {max}");
        // The BBSK catalogue (24 types, 1,317 attributes) is the one past the switch.
        let brief: Vec<&str> = cards
            .iter()
            .filter(|(_, g, w)| g < w)
            .map(|(slug, ..)| slug.as_str())
            .collect();
        assert_eq!(brief, ["6b4yqhzqieutqcoeew5rnxrx5udqeert"]);
        assert!(whole <= 30_000, "{whole}");
    }
}
