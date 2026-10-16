//! A unit a person reads beside each `unitCode` of a tool result (T-4373).
//!
//! NGSI-LD writes a quantity's unit as a UN/CEFACT code (`GQ`), which a model reading the answer
//! takes for "no unit". The MCP tool results gain `"unit": "µg/m³"` from jc-core's Rec 20 table
//! beside every code it knows; the stored entity and the NGSI-LD answers are not changed.

use jc_core::units;
use serde_json::Value;

/// Adds `unit` beside every known `unitCode` of a JSON-RPC tool answer: in its
/// `structuredContent` and in each text item that is JSON.
pub fn readable(answer: &mut Value) {
    let Some(result) = answer.get_mut("result") else {
        return;
    };
    if let Some(structured) = result.get_mut("structuredContent") {
        name_units(structured);
    }
    let Some(items) = result.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        let Some(text) = item.get("text").and_then(Value::as_str) else {
            continue;
        };
        let Ok(mut parsed) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if name_units(&mut parsed) {
            item["text"] = Value::String(parsed.to_string());
        }
    }
}

/// Whether any unit was added below `value`.
fn name_units(value: &mut Value) -> bool {
    match value {
        Value::Object(map) => {
            let mut added = false;
            if !map.contains_key("unit") {
                let unit = map
                    .get("unitCode")
                    .and_then(Value::as_str)
                    .and_then(units::lookup)
                    .map(|unit| {
                        if unit.symbol.is_empty() {
                            unit.name
                        } else {
                            unit.symbol
                        }
                    });
                if let Some(unit) = unit {
                    map.insert("unit".to_owned(), Value::String(unit.to_owned()));
                    added = true;
                }
            }
            for child in map.values_mut() {
                added |= name_units(child);
            }
            added
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |added, item| name_units(item) | added),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn answer(payload: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 1, "result": {
            "content": [{ "type": "text", "text": payload.to_string() }],
            "structuredContent": payload,
        } })
    }

    #[test]
    fn a_known_code_gains_its_symbol_in_both_halves_and_an_unknown_one_nothing() {
        let mut sent = answer(json!({ "entities": [{
            "pm10": { "type": "Property", "value": 8.93, "unitCode": "GQ" },
            "co": { "type": "Property", "value": 0.3, "unitCode": "GP" },
            "odd": { "type": "Property", "value": 1, "unitCode": "ZZZ9" },
            "name": { "type": "Property", "value": "unitCode" },
        }] }));
        readable(&mut sent);
        let text: Value =
            serde_json::from_str(sent["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        for entity in [
            &sent["result"]["structuredContent"]["entities"][0],
            &text["entities"][0],
        ] {
            assert_eq!(entity["pm10"]["unit"], "µg/m³");
            assert_eq!(entity["pm10"]["unitCode"], "GQ");
            assert_eq!(entity["co"]["unit"], "mg/m³");
            assert!(entity["odd"].get("unit").is_none());
            assert!(entity["name"].get("unit").is_none());
        }
    }

    #[test]
    fn a_unit_already_there_a_text_answer_and_an_error_are_left_alone() {
        let mut own = answer(json!({ "pm10": { "unitCode": "GQ", "unit": "ug/m3" } }));
        readable(&mut own);
        assert_eq!(own["result"]["structuredContent"]["pm10"]["unit"], "ug/m3");

        let mut words =
            json!({ "result": { "content": [{ "type": "text", "text": "unitCode GQ" }] } });
        let before = words.clone();
        readable(&mut words);
        assert_eq!(words, before);

        let mut error = json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32602 } });
        let before = error.clone();
        readable(&mut error);
        assert_eq!(error, before);
    }
}
