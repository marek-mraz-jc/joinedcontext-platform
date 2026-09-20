"""The `@context` an NGSI-LD payload expands with (T-0169, DM-04, DM-05, DM-16)."""

from __future__ import annotations

import json

import pytest
from rdflib import Graph, Literal, URIRef

from common import ModelError
from gen_context import compile_context

BB = "https://banskabystrica.sk/ns/"


def _expanded(document: dict, entity: dict) -> Graph:
    """The entity as RDF, expanded with the generated context, the way a consumer reads it."""
    payload = {"@context": document["@context"], **entity}
    return Graph().parse(data=json.dumps(payload), format="json-ld")


def test_every_term_binds_the_iri_the_model_declares(senzor):
    context = compile_context(senzor)["@context"]
    assert context["temperature"]["@id"] == "bb:temperature"
    assert context["AirQualityObserved"]["@id"] == "bb:AirQualityObserved"
    # Inherited from ngsi-ld-core, and still ETSI's IRI (DM-09).
    assert context["observedAt"]["@id"] == "ngsi-ld:observedAt"


def test_a_relationship_expands_to_an_iri_and_not_to_a_string(senzor):
    document = compile_context(senzor)
    assert document["@context"]["refDevice"]["@type"] == "@id"

    device = "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:d1"
    graph = _expanded(document, {"type": "AirQualityObserved", "refDevice": device})
    objects = list(graph.objects(None, URIRef(f"{BB}refDevice")))
    assert objects == [URIRef(device)], "a relationship nobody can follow is a string"


def test_an_inline_object_is_not_typed_as_an_iri(senzor):
    """LinkML types every class-ranged slot `@type: @id`, which is right for a Relationship
    and wrong for a JsonProperty: an imported `address` carries the object inline, and a
    consumer that reads the context as written resolves it as an IRI and loses the value.
    The `ngsi_ld_kind` annotation decides (DM-05)."""
    document = compile_context(senzor)
    assert document["@context"]["address"].get("@type") != "@id"

    address = {"streetAddress": "Namestie SNP 1"}
    graph = _expanded(document, {"type": "AirQualityObserved", "address": address})
    objects = list(graph.objects(None, URIRef(f"{BB}address")))
    assert objects and not isinstance(objects[0], URIRef), "the object was read as a reference"


def test_a_language_property_expands_to_one_literal_per_locale(senzor):
    document = compile_context(senzor)
    assert document["@context"]["label"]["@container"] == "@language"

    graph = _expanded(document, {"type": "AirQualityObserved", "label": {"sk": "Senzor", "en": "Sensor"}})
    labels = set(graph.objects(None, URIRef(f"{BB}label")))
    assert labels == {Literal("Senzor", lang="sk"), Literal("Sensor", lang="en")}


def test_a_vocab_property_expands_its_value_to_a_term_and_not_to_a_string(senzor):
    """DM-05, §1.1: a VocabProperty names a term of a vocabulary. Without `@type: @vocab` the
    value stays the five characters it is written with, and a partner reading `pm10` has
    nothing to look up."""
    document = compile_context(senzor)
    assert document["@context"]["pollutant"]["@type"] == "@vocab"

    graph = _expanded(document, {"type": "AirQualityObserved", "pollutant": "pm10"})
    objects = list(graph.objects(None, URIRef(f"{BB}pollutant")))
    assert objects == [URIRef(f"{BB}pm10")], f"the term did not resolve: {objects}"


def test_a_list_property_keeps_the_order_the_model_gave_it(senzor):
    """DM-05, §1.1: a JSON-LD array without `@list` is a set, and a consumer may reorder it.
    The kind says the order is part of the meaning, so the container has to say so too."""
    document = compile_context(senzor)
    assert document["@context"]["measuredAt"]["@container"] == "@list"

    graph = _expanded(
        document, {"type": "AirQualityObserved", "measuredAt": [0.5, 2.0, 10.0]}
    )
    # An RDF list is a chain of rdf:first/rdf:rest, which is exactly what `@list` produces and
    # what a plain array does not.
    from rdflib import RDF

    head = next(graph.objects(None, URIRef(f"{BB}measuredAt")))
    order = []
    node = head
    while node != RDF.nil:
        order.append(next(graph.objects(node, RDF.first)).toPython())
        node = next(graph.objects(node, RDF.rest))
    assert order == [0.5, 2.0, 10.0], f"the order was lost: {order}"


def test_an_opaque_document_stays_a_value_and_is_not_walked_into(senzor):
    """DM-05, §1.1: `@json` says the document is a value this model does not describe, so
    expansion does not invent a term for every key inside it."""
    document = compile_context(senzor)
    assert document["@context"]["address"]["@type"] == "@json"

    graph = _expanded(
        document,
        {"type": "AirQualityObserved", "address": {"streetAddress": "Namestie SNP 1"}},
    )
    assert not list(graph.objects(None, URIRef(f"{BB}streetAddress"))), (
        "a key of the opaque document became a term of its own"
    )


def test_a_term_definition_carries_only_json_ld_keywords(senzor):
    """JSON-LD 1.1 refuses a term definition with a key it does not know, and a refused
    context expands to nothing at all."""
    allowed = {"@id", "@type", "@container", "@language", "@context", "@index", "@nest",
               "@prefix", "@protected", "@propagate", "@reverse"}
    for term, definition in compile_context(senzor)["@context"].items():
        if isinstance(definition, dict):
            assert set(definition) <= allowed, f"term '{term}' carries {set(definition) - allowed}"


def test_minting_a_term_under_a_foreign_namespace_is_refused(squatted):
    with pytest.raises(ModelError, match="ourOwnIdea"):
        compile_context(squatted)


def test_an_upstream_term_keeps_its_own_namespace(senzor):
    """The refusal is about minting, not about citing: ngsi-ld-core carries ETSI IRIs and
    says where they come from, so it compiles."""
    assert compile_context(senzor)["@context"]["observedAt"]["@id"].startswith("ngsi-ld:")


def test_the_generator_version_is_recorded(senzor):
    assert compile_context(senzor)["x-generator-version"].startswith("linkml-")
