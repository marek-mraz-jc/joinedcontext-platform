"""The example entity DM-02 commits and DM-21 constrains (T-0415, DM-02, DM-21, DM-43)."""

from __future__ import annotations

import jsonschema
import pytest

from common import ModelError, load
from gen_context import compile_context
from gen_example import compile_example, entity_class
from gen_json_schema import compile_schema


def test_the_example_validates_against_the_generated_json_schema(senzor):
    """DM-21, asserted here as well as inside the generator: this is the property, not a detail."""
    example = compile_example(senzor)
    schema = compile_schema(senzor)
    against = {**schema, "$ref": "#/definitions/AirQualityObserved"}
    against.pop("additionalProperties", None)

    jsonschema.Draft7Validator(against).validate(example)


def test_every_member_is_a_term_the_generated_context_defines(senzor):
    example = compile_example(senzor)
    terms = compile_context(senzor)["@context"]
    unmapped = [k for k in example if k not in ("id", "type") and k not in terms]
    assert unmapped == [], "an unmapped member expands as opaque data (DM-21)"


def test_the_entity_id_follows_the_platform_scheme(senzor):
    """PF-42: four segments, the organisation's own domain, and a segment saying it is a sample."""
    example = compile_example(senzor)
    assert example["id"] == "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:example:1"
    assert example["type"] == "AirQualityObserved"


def test_a_relationship_carries_an_entity_id_and_not_an_inline_object(senzor):
    value = compile_example(senzor)["refDevice"]
    assert isinstance(value, str) and value.startswith("urn:ngsi-ld:")


def test_an_inline_object_carries_the_shape_of_its_class(senzor):
    assert compile_example(senzor)["address"] == {"streetAddress": "streetAddress"}


def test_an_enum_slot_takes_a_permissible_value(senzor):
    assert compile_example(senzor)["reliability"] == "low"


def test_a_geoproperty_carries_a_geojson_geometry_object(senzor):
    """The simplified form of a GeoProperty is a geometry, not a string holding one (T-0416)."""
    assert compile_example(senzor)["location"] == {"type": "Point", "coordinates": [0.0, 0.0]}


def test_a_languageproperty_carries_a_language_map(senzor):
    assert compile_example(senzor)["label"] == {"en": "label"}


def test_nothing_in_the_example_moves_between_two_runs(senzor):
    """A committed artifact is compared byte for byte, so a clock in it is a daily red lane."""
    assert compile_example(senzor) == compile_example(senzor)
    assert compile_example(senzor)["observedAt"] == "2026-01-01T00:00:00Z"


def test_a_slot_that_declares_its_own_example_keeps_it(tmp_path):
    source = tmp_path / "authored.linkml.yaml"
    source.write_text(
        "id: https://example.org/ns/authored\n"
        "name: authored\n"
        "prefixes: {linkml: 'https://w3id.org/linkml/', ex: 'https://example.org/ns/'}\n"
        "default_prefix: ex\n"
        "default_range: string\n"
        "imports: [linkml:types, ngsi-ld-core]\n"
        "classes:\n"
        "  Station:\n"
        "    class_uri: ex:Station\n"
        "    is_a: Entity\n"
        "    attributes:\n"
        "      name:\n"
        "        slot_uri: ex:name\n"
        "        examples:\n"
        "          - value: Kalevankatu\n",
        encoding="utf-8",
    )
    assert compile_example(str(source))["name"] == "Kalevankatu"


def test_a_model_with_no_entity_class_is_refused(tmp_path):
    """An example of nothing is worse than no example: the model declares no NGSI-LD type."""
    source = tmp_path / "inline-only.linkml.yaml"
    source.write_text(
        "id: https://example.org/ns/inline\n"
        "name: inline\n"
        "prefixes: {linkml: 'https://w3id.org/linkml/', ex: 'https://example.org/ns/'}\n"
        "default_prefix: ex\n"
        "default_range: string\n"
        "imports: [linkml:types]\n"
        "classes:\n"
        "  Address:\n"
        "    class_uri: ex:Address\n"
        "    attributes:\n"
        "      street:\n"
        "        slot_uri: ex:street\n",
        encoding="utf-8",
    )
    with pytest.raises(ModelError, match="Entity"):
        compile_example(str(source))


def test_the_first_entity_class_is_the_one_the_example_is_of(senzor):
    """One example file per model (DM-02); the Markdown page documents every class."""
    assert entity_class(load(senzor)).name == "AirQualityObserved"


def test_a_bound_or_a_simple_pattern_shapes_the_example(tmp_path):
    """DM-21: a slot with `minimum_value` or a pattern gets an example inside them, so a model
    the editor or inference constrained still validates against its own schema."""
    source = tmp_path / "bounded.linkml.yaml"
    source.write_text(
        """
id: https://example.org/models/Bounded
name: Bounded
prefixes: {linkml: https://w3id.org/linkml/}
default_range: string
imports: [linkml:types, ngsi-ld-core]
classes:
  Reading:
    is_a: Entity
    slots: [pm10, share, plate]
slots:
  pm10: {range: integer, minimum_value: 12, maximum_value: 40}
  share: {range: float, maximum_value: -1.5}
  plate: {range: string, pattern: '^[A-Z]{2}[0-9]{3}[A-Z]{2}$'}
"""
    )

    example = compile_example(source)

    assert example["pm10"] == 12
    assert example["share"] == -1.5
    assert example["plate"] == "AA000AA"

    # A pattern the generator cannot spell is the author's to give an example for, and the
    # refusal names the slot.
    source.write_text(source.read_text().replace("slots: [pm10, share, plate]", "slots: [free]") + "  free: {range: string, pattern: '^(a|b)+$'}\n")
    with pytest.raises(ModelError, match="free"):
        compile_example(source)
