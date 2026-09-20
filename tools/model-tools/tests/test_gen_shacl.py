"""The shapes a consumer validates our responses with (T-0170, DM-28, DM-43, DM-46).

DM-46 asks for exactly this proof: the SHACL artifact must work in a consumer's own validator
against `ngsi-ld/v1` responses expanded with the served `@context`. Every case here therefore
expands a real entity with the generated context and hands both graphs to pySHACL.
"""

from __future__ import annotations

import json

from pyshacl import validate
from rdflib import Graph

from gen_context import compile_context
from gen_rdf_artifacts import compile_owl, compile_shacl

DEVICE = "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:d1"
SENSOR = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:senzor-01"


def _graph(senzor: str, entity: dict) -> Graph:
    """One entity as a consumer sees it: JSON from the API, expanded with the served context."""
    context = compile_context(senzor)["@context"]
    # `id` and `type` are JSON-LD keywords in the NGSI-LD core context, which every response
    # carries alongside the model context.
    payload = {"@context": [{"id": "@id", "type": "@type"}, context], **entity}
    return Graph().parse(data=json.dumps(payload), format="json-ld")


def _conforms(senzor: str, entity: dict) -> tuple[bool, str]:
    shapes = Graph().parse(data=compile_shacl(senzor), format="turtle")
    conforms, _, text = validate(_graph(senzor, entity), shacl_graph=shapes, advanced=True)
    return conforms, text


def _reading(**overrides) -> dict:
    entity = {
        "id": SENSOR,
        "type": "AirQualityObserved",
        "temperature": 12.5,
        "refDevice": DEVICE,
        "label": {"sk": "Senzor", "en": "Sensor"},
    }
    entity.update(overrides)
    return entity


def test_a_valid_reading_conforms(senzor):
    conforms, report = _conforms(senzor, _reading())
    assert conforms, report


def test_a_reading_without_its_required_slot_does_not_conform(senzor):
    entity = _reading()
    del entity["temperature"]
    conforms, report = _conforms(senzor, entity)
    assert not conforms
    assert "temperature" in report


def test_an_undeclared_attribute_does_not_conform(senzor):
    """DM-28: the shapes are closed, so an attribute the model never declared is a failure."""
    conforms, report = _conforms(senzor, _reading(smuggled="whatever"))
    assert not conforms
    assert "closed" in report.lower()


def test_the_shapes_agree_with_the_context_about_relationships(senzor):
    """A relationship is an IRI in both artifacts or the pair is useless (DM-43).

    There is no negative case to write here: `@type: @id` in the context coerces whatever the
    payload carries into an IRI, so a literal never reaches the validator. The failure this
    guards against is the opposite one, where LinkML derives `sh:datatype xsd:string` from the
    range and every relationship of a real response is reported as a violation.
    """
    shapes = Graph().parse(data=compile_shacl(senzor), format="turtle")
    turtle = shapes.serialize(format="turtle")
    device_shape = [line for line in turtle.splitlines() if "refDevice" in line]
    assert device_shape, "the shapes say nothing about refDevice"

    conforms, report = _conforms(senzor, _reading(refDevice=DEVICE))
    assert conforms, report


def test_an_open_world_class_accepts_what_it_does_not_declare(senzor):
    """A class that opted in stays open; every other shape stays closed."""
    conforms, report = _conforms(
        senzor, {"id": "urn:ngsi-ld:FreeForm:banskabystrica.sk:ovzdusie:1", "type": "FreeForm",
                 "whateverTheSensorSent": "42"}
    )
    assert conforms, report


def test_the_wrong_datatype_does_not_conform(senzor):
    conforms, report = _conforms(senzor, _reading(temperature="warm"))
    assert not conforms
    assert "temperature" in report


def test_the_owl_ontology_parses_and_carries_the_classes(senzor):
    graph = Graph().parse(data=compile_owl(senzor), format="turtle")
    classes = {str(s) for s in graph.subjects()}
    assert "https://banskabystrica.sk/ns/AirQualityObserved" in classes


def test_the_shapes_record_the_generator_version(senzor):
    assert "generator_version" in compile_shacl(senzor)
