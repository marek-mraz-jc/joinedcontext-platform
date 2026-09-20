"""The JSON Schema the gateway validates writes against (T-0168, DM-02, DM-03, TS-18)."""

from __future__ import annotations

import json

import pytest
from jsonschema import Draft7Validator

from gen_json_schema import compile_schema, main

#: Keywords that exist only in 2019-09 and later. A draft-07 validator ignores them silently,
#: so one of these in a generated schema is a constraint nobody enforces.
LATER_DRAFT_KEYWORDS = (
    "$defs",
    "unevaluatedProperties",
    "unevaluatedItems",
    "$anchor",
    "$recursiveRef",
    "$dynamicRef",
    "dependentRequired",
    "dependentSchemas",
    "prefixItems",
)


def _keys(node) -> set[str]:
    if isinstance(node, dict):
        return set(node) | {k for value in node.values() for k in _keys(value)}
    if isinstance(node, list):
        return {k for item in node for k in _keys(item)}
    return set()


def test_the_schema_is_valid_draft_07(senzor):
    schema = compile_schema(senzor)
    assert schema["$schema"] == "http://json-schema.org/draft-07/schema#"
    Draft7Validator.check_schema(schema)


def test_no_keyword_from_a_later_draft_survives(senzor):
    schema = compile_schema(senzor)
    present = _keys(schema) & set(LATER_DRAFT_KEYWORDS)
    assert present == set(), f"these keywords are not draft-07: {sorted(present)}"


def test_references_point_at_definitions(senzor):
    schema = compile_schema(senzor)
    text = json.dumps(schema)
    assert "#/$defs/" not in text
    assert "definitions" in schema


def test_a_valid_entity_validates_and_an_invalid_one_does_not(senzor):
    schema = compile_schema(senzor)
    validator = Draft7Validator({**schema, "$ref": "#/definitions/AirQualityObserved"})
    entity = {
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:senzor-01",
        "type": "AirQualityObserved",
        "temperature": 12.5,
        "reliability": "high",
    }
    assert list(validator.iter_errors(entity)) == []

    # DM-28: an attribute the model does not declare is refused, it is not an extension.
    undeclared = {**entity, "smuggled": 1}
    assert list(validator.iter_errors(undeclared)) != []

    # The enum is what the gateway checks a value against.
    assert list(validator.iter_errors({**entity, "reliability": "excellent"})) != []


def test_the_ngsi_ld_kind_of_every_slot_travels_with_the_schema(senzor):
    properties = compile_schema(senzor)["definitions"]["AirQualityObserved"]["properties"]
    assert properties["refDevice"]["x-ngsi-ld-kind"] == "Relationship"
    assert properties["label"]["x-ngsi-ld-kind"] == "LanguageProperty"
    assert properties["temperature"]["x-ngsi-ld-kind"] == "Property"
    # Inherited from the shared ngsi-ld-core import (DM-09).
    assert properties["location"]["x-ngsi-ld-kind"] == "GeoProperty"


def test_a_language_map_and_a_geometry_are_objects_and_not_strings(senzor):
    """T-0416: the range of both is a string; the JSON shape ETSI 9.3.2.3 defines is an object,
    and a schema that says otherwise refuses exactly what a conforming partner sends."""
    schema = compile_schema(senzor)
    validator = Draft7Validator({**schema, "$ref": "#/definitions/AirQualityObserved"})
    entity = {
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:senzor-01",
        "type": "AirQualityObserved",
        "temperature": 12.5,
    }

    conforming = {
        **entity,
        "label": {"sk": "Senzor pri škole", "en": "Sensor by the school"},
        "location": {"type": "Point", "coordinates": [19.15, 48.73]},
    }
    assert list(validator.iter_errors(conforming)) == []

    # And the plain string the old schema accepted is now what fails.
    assert list(validator.iter_errors({**entity, "label": "Sensor"})) != []
    assert list(validator.iter_errors({**entity, "location": "POINT(19.15 48.73)"})) != []
    # A geometry without coordinates, and one of a type no geo-query operator covers.
    assert list(validator.iter_errors({**entity, "location": {"type": "Point"}})) != []
    assert list(
        validator.iter_errors(
            {**entity, "location": {"type": "GeometryCollection", "coordinates": []}}
        )
    ) != []


def test_a_language_map_keeps_the_description_of_its_slot(senzor):
    """The shape replaces the type, not the documentation the editor and the docs page read."""
    label = compile_schema(senzor)["definitions"]["AirQualityObserved"]["properties"]["label"]
    assert label["description"] == "What the sensor is called, per locale."
    assert label["additionalProperties"] == {"type": "string"}


def test_the_unit_travels_with_the_schema(senzor):
    temperature = compile_schema(senzor)["definitions"]["AirQualityObserved"]["properties"][
        "temperature"
    ]
    unit = temperature["x-unit"]
    assert unit["ucumCode"] == "Cel"
    assert unit["symbol"] == "°C"
    # The UN/CEFACT common code travels in exact_mappings (DM-06).
    assert "unece:CEL" in unit["exactMappings"]


def test_the_generator_version_is_recorded(senzor):
    assert compile_schema(senzor)["x-generator-version"].startswith("linkml-")


def test_the_cli_writes_the_file_it_is_given(senzor, tmp_path):
    out = tmp_path / "schema.json"
    assert main([senzor, "-o", str(out)]) == 0
    Draft7Validator.check_schema(json.loads(out.read_text()))


def test_a_slot_with_an_unknown_kind_is_refused(tmp_path):
    source = tmp_path / "bad.yaml"
    source.write_text(
        "id: https://banskabystrica.sk/ns/bad\n"
        "name: bad\n"
        "prefixes: {linkml: 'https://w3id.org/linkml/', bb: 'https://banskabystrica.sk/ns/'}\n"
        "default_prefix: bb\n"
        "default_range: string\n"
        "imports: [linkml:types]\n"
        "classes: {Reading: {class_uri: 'bb:Reading', slots: [odd]}}\n"
        "slots: {odd: {slot_uri: 'bb:odd', annotations: {ngsi_ld_kind: Telepathy}}}\n"
    )
    with pytest.raises(Exception, match="Telepathy"):
        compile_schema(str(source))
