"""The schema a broker Tenant's views read (T-3703): the broker refuses any key outside its subset."""

from __future__ import annotations

import json

import pytest

from common import ModelError, imported
from gen_broker_schema import BUILTINS, compile_broker_schema, main

#: The keys Antares `linkml-model` (`schema.rs`) accepts; every struct there denies unknown fields.
SCHEMA_KEYS = {"id", "name", "version", "prefixes", "default_prefix", "classes", "slots", "enums"}
PREFIX_KEYS = {"prefix_prefix", "prefix_reference"}
CLASS_KEYS = {"name", "class_uri", "is_a", "slots", "slot_usage", "annotations", "description"}
SLOT_KEYS = {
    "name",
    "slot_uri",
    "range",
    "required",
    "multivalued",
    "identifier",
    "inlined",
    "inverse",
    "unit",
    "annotations",
    "description",
    "rank",
    "title",
}
UNIT_KEYS = {"ucum_code", "symbol", "exact_mappings"}
ENUM_KEYS = {"name", "description", "permissible_values"}
VALUE_KEYS = {"text", "description", "meaning"}

#: A custom type, a narrowed inherited slot, a mixin, two same-named attributes that differ, and
#: annotations of every YAML scalar type.
NARROWED = """
id: https://example.org/ns/narrowed
name: narrowed
prefixes:
  linkml: https://w3id.org/linkml/
  ex: https://example.org/ns/
default_prefix: ex
default_range: string
imports:
  - linkml:types
  - ngsi-ld-core
types:
  Celsius:
    typeof: float
    uri: xsd:float
  Reading:
    typeof: Celsius
    uri: xsd:float
classes:
  Station:
    is_a: Entity
    slots: [capacity, temperature]
    annotations:
      odata_insertable: false
      weight: 3
  BigStation:
    is_a: Station
    mixins: [Named]
    slot_usage:
      capacity:
        required: true
  Named:
    mixin: true
    slots: [title]
  Left:
    attributes:
      code: {range: integer}
  Right:
    attributes:
      code: {range: string, required: true}
slots:
  capacity: {range: integer}
  temperature:
    range: Reading
    annotations:
      odata_computed: true
  title: {}
"""


def check_subset(document: dict) -> None:
    """Fail on any key the broker's parser would refuse."""

    def slot(entry: dict, where: str) -> None:
        assert set(entry) <= SLOT_KEYS, (where, set(entry) - SLOT_KEYS)
        assert set(entry.get("unit", {})) <= UNIT_KEYS, where
        for tag, a in entry.get("annotations", {}).items():
            assert a == {"tag": tag, "value": a["value"]} and isinstance(a["value"], str), (where, tag)

    assert set(document) <= SCHEMA_KEYS, set(document) - SCHEMA_KEYS
    for key, p in document["prefixes"].items():
        assert set(p) == PREFIX_KEYS and p["prefix_prefix"] == key
    names = set(document["classes"]) | set(document["enums"])
    for key, entry in document["slots"].items():
        slot(entry, f"slot {key}")
        assert entry["range"] in names or entry["range"] in BUILTINS, (key, entry["range"])
    for key, cls in document["classes"].items():
        assert set(cls) <= CLASS_KEYS, (key, set(cls) - CLASS_KEYS)
        assert cls.get("is_a") is None or cls["is_a"] in document["classes"], key
        assert set(cls["slots"]) <= set(document["slots"]), key
        for name, usage in cls.get("slot_usage", {}).items():
            assert name in cls["slots"], (key, name)
            slot(usage, f"class {key} slot_usage {name}")
    for key, enum in document["enums"].items():
        assert set(enum) <= ENUM_KEYS, key
        assert all(set(v) <= VALUE_KEYS for v in enum["permissible_values"].values()), key


def test_the_fixture_is_in_the_subset_with_its_import_merged(senzor):
    document = compile_broker_schema(senzor)

    check_subset(document)
    assert "source_file" not in json.dumps(document)
    assert document["id"] == "https://banskabystrica.sk/ns/senzor" and document["default_prefix"] == "bb"
    # ngsi-ld-core is merged: its class, its slots and its prefix, so `is_a: Entity` resolves.
    assert document["classes"]["Entity"]["class_uri"] == "ngsi-ld:Entity"
    assert document["slots"]["id"]["identifier"] is True
    assert document["prefixes"]["ngsi-ld"]["prefix_reference"] == "https://uri.etsi.org/ngsi-ld/"
    observed = document["classes"]["AirQualityObserved"]
    assert observed["is_a"] == "Entity" and {"temperature", "id", "location"} <= set(observed["slots"])
    # An attribute is a top-level slot the class lists.
    assert document["classes"]["Address"]["slots"] == ["streetAddress"]
    assert document["slots"]["streetAddress"]["slot_uri"] == "bb:streetAddress"
    temperature = document["slots"]["temperature"]
    assert temperature["unit"] == {"ucum_code": "Cel", "symbol": "°C", "exact_mappings": ["unece:CEL", "qudt-unit:DEG_C"]}
    assert temperature["annotations"]["ngsi_ld_kind"] == {"tag": "ngsi_ld_kind", "value": "Property"}
    assert document["slots"]["reliability"]["range"] == "ReliabilityLevel"
    assert document["classes"]["FreeForm"]["annotations"]["open_world"]["value"] == "true"


def test_a_type_narrowing_and_mixin_become_what_the_broker_reads():
    document = compile_broker_schema(NARROWED)

    check_subset(document)
    # A type is walked down `typeof` to the built-in it specialises.
    assert document["slots"]["temperature"]["range"] == "float"
    # A class's narrowing holds only what differs, and a subclass lists inherited and mixed-in slots.
    big = document["classes"]["BigStation"]
    assert big["slot_usage"] == {"capacity": {"name": "capacity", "required": True}}
    assert {"capacity", "temperature", "title", "id"} <= set(big["slots"])
    assert "slot_usage" not in document["classes"]["Station"]
    # Two attributes of one name: one top-level slot, the other class's difference as its usage.
    left, right = document["classes"]["Left"], document["classes"]["Right"]
    top = document["slots"]["code"]
    differs = left.get("slot_usage", {}).get("code") or right.get("slot_usage", {}).get("code")
    assert differs and differs["range"] != top["range"]
    # Annotation values are text, booleans as the broker's flags read them.
    station = document["classes"]["Station"]["annotations"]
    assert station["odata_insertable"]["value"] == "false" and station["weight"]["value"] == "3"
    assert document["slots"]["temperature"]["annotations"]["odata_computed"]["value"] == "true"


def test_a_range_the_broker_cannot_hold_is_refused_with_its_name():
    unknown = NARROWED.replace("typeof: float\n    uri: xsd:float\n  Reading", "uri: xsd:float\n  Reading")
    with pytest.raises(ModelError, match="range 'Reading' is not a class, an enum or a type based on"):
        compile_broker_schema(unknown)


def test_a_platform_model_import_is_merged_in():
    station = """
id: https://example.org/ns/stations
name: stations
prefixes: {linkml: https://w3id.org/linkml/, st: https://example.org/ns/stations/}
default_prefix: st
imports: [linkml:types, ngsi-ld-core]
classes:
  Station: {is_a: Entity, slots: [capacity]}
slots:
  capacity: {range: integer}
"""
    bikes = """
id: https://example.org/ns/bikes
name: bikes
prefixes: {linkml: https://w3id.org/linkml/, ex: https://example.org/ns/}
default_prefix: ex
imports: [linkml:types, ngsi-ld-core, org.stations.v1]
classes:
  BikeStation: {is_a: Station}
"""
    with imported({"org.stations.v1": station}):
        document = compile_broker_schema(bikes)

    check_subset(document)
    assert document["classes"]["Station"]["slots"] == ["capacity", "id", "type", "location", "observedAt"]
    assert "capacity" in document["classes"]["BikeStation"]["slots"]
    assert document["prefixes"]["st"]["prefix_reference"] == "https://example.org/ns/stations/"


def test_the_command_line_writes_the_document_and_reports_a_refusal(senzor, tmp_path, capsys):
    out = tmp_path / "senzor.broker.json"
    assert main([senzor, "-o", str(out)]) == 0
    assert json.loads(out.read_text())["name"] == "senzor"

    bad = tmp_path / "bad.linkml.yaml"
    bad.write_text(NARROWED.replace("typeof: float\n    uri: xsd:float\n  Reading", "uri: xsd:float\n  Reading"))
    assert main([str(bad)]) == 1
    assert "range 'Reading'" in capsys.readouterr().err


def test_the_display_order_rank_title_and_label_reach_the_broker():
    """T-4672 (DM-98, DM-100): `rank` stays a number, `title` text, a label's language map JSON text."""
    model = """
id: https://example.org/ns/events
name: events
prefixes:
  linkml: https://w3id.org/linkml/
default_range: string
imports: [linkml:types]
slots:
  name:
    title: Name
  startDate: {}
classes:
  Event:
    slots: [name, startDate]
    slot_usage:
      startDate:
        rank: 1
        annotations:
          summary: true
          label: { tag: label, value: { sk: "Začiatok", en: "Start" } }
"""
    document = compile_broker_schema(model)
    check_subset(document)
    usage = document["classes"]["Event"]["slot_usage"]["startDate"]
    assert usage["rank"] == 1 and isinstance(usage["rank"], int)
    assert usage["annotations"]["summary"]["value"] == "true"
    assert json.loads(usage["annotations"]["label"]["value"]) == {"sk": "Začiatok", "en": "Start"}
    assert document["slots"]["name"]["title"] == "Name"
