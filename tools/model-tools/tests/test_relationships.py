"""Relationships strict like foreign keys in every artifact (T-2739, DM-64…DM-72).

One School ↔ User pair in each of the four cardinalities, against golden JSON Schemas; one model
per broken rule, refused by every generator and by `/generate` one entry per rule; the SHACL
shapes in pySHACL against good and bad entities; and the OWL the pair renders.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
import yaml
from pyshacl import validate
from rdflib import Graph, URIRef
from rdflib.namespace import OWL, RDF

from common import RelationshipError
from gen_context import compile_context
from gen_json_schema import compile_schema
from gen_rdf_artifacts import compile_owl, compile_shacl
from service import artifacts

FIXTURES = Path(__file__).parent / "fixtures" / "relationships"
BASE = yaml.safe_load((FIXTURES / "school.linkml.yaml").read_text())

#: Read from School (the source, carrying on_delete) to User: School.users, User.school.
FLAGS = {
    "one-to-one": (False, False),
    "one-to-many": (True, False),
    "many-to-one": (False, True),
    "many-to-many": (True, True),
}

SCHOOL = "urn:ngsi-ld:School:rozvoj.sk:schools:gymnazium"
OTHER_SCHOOL = "urn:ngsi-ld:School:rozvoj.sk:schools:zakladna"
USER = "urn:ngsi-ld:User:rozvoj.sk:schools:jana"


def model(cardinality: str = "one-to-many", edit=None) -> str:
    """The School ↔ User model as text, in one cardinality, with `edit` applied to its dict."""
    doc = json.loads(json.dumps(BASE))
    users_many, school_many = FLAGS[cardinality]
    doc["slots"]["users"]["multivalued"] = users_many
    doc["slots"]["school"]["multivalued"] = school_many
    if edit is not None:
        edit(doc)
    return yaml.safe_dump(doc, sort_keys=False)


def relationship_part(schema: dict) -> dict:
    """What the golden files hold: the two classes' relationship properties and required lists."""
    return {
        name: {
            "properties": {
                slot: prop
                for slot, prop in schema["definitions"][name]["properties"].items()
                if slot in ("users", "school")
            },
            "required": schema["definitions"][name].get("required", []),
        }
        for name in ("School", "User")
    }


@pytest.mark.parametrize("cardinality", list(FLAGS))
def test_the_json_schema_of_each_cardinality_is_the_golden_one(cardinality):
    """DM-67, DM-72: the stored end is a URN of the target type, one or a list; the computed end
    is not in the schema at all; the gateway reads the rule from `x-ngsi-ld-relationship`."""
    rendered = relationship_part(compile_schema(model(cardinality)))
    golden = json.loads((FIXTURES / f"{cardinality}.schema.json").read_text())
    assert rendered == golden


def test_a_required_many_end_takes_at_least_one_target():
    def required(doc):
        doc["slots"]["users"]["required"] = True

    users = compile_schema(model("many-to-many", required))["definitions"]["School"]
    assert users["properties"]["users"]["minItems"] == 1
    assert "users" in users["required"]


# One model per rule of DM-68, each as an edit of the well-formed one-to-many pair.
BROKEN = {
    "range-not-a-class": lambda doc: doc["slots"]["users"].update(range="Teacher"),
    "class-range-not-relationship": lambda doc: doc["slots"]["school"]["annotations"].update(ngsi_ld_kind="Property"),
    "primitive-range": lambda doc: doc["slots"]["users"].update(range="integer"),
    "inverse-missing": lambda doc: doc["slots"]["users"].pop("inverse"),
    "inverse-not-reciprocal": lambda doc: doc["slots"]["school"].update(inverse="name"),
    "slot-in-two-relationships": lambda doc: doc["classes"].update(Club={"is_a": "Entity", "slots": ["users"]}),
    "required-on-computed-end": lambda doc: doc["slots"]["users"].update(required=True),
    "on-delete-unknown": lambda doc: doc["slots"]["users"]["annotations"].update(on_delete="vanish"),
    "on-delete-on-both-ends": lambda doc: doc["slots"]["school"]["annotations"].update(on_delete="cascade"),
}


@pytest.mark.parametrize("rule", list(BROKEN))
def test_every_generator_refuses_a_broken_relationship_naming_the_rule(rule):
    """DM-68: an error, never a warning, naming the slot and the rule."""
    source = model("one-to-many", BROKEN[rule])
    for render in (compile_schema, compile_context, compile_shacl, compile_owl):
        with pytest.raises(RelationshipError) as refused:
            render(source)
        assert any(f": {rule}: " in problem for problem in refused.value.problems), refused.value.problems
        assert all(problem.startswith(("slots.", "classes.")) for problem in refused.value.problems)


def test_generate_answers_each_broken_rule_as_its_own_error_and_no_artifact():
    """DM-68, API/01: the Portal's save route shows these one per entry."""
    def two(doc):
        doc["slots"]["users"]["annotations"]["on_delete"] = "vanish"
        doc["slots"]["users"]["required"] = True

    answer = artifacts(model("one-to-many", two))
    assert sorted(error.split(": ")[1] for error in answer["errors"]) == ["on-delete-unknown", "required-on-computed-end"]
    assert set(answer) == {"generatorVersion", "errors"}
    assert artifacts(model("one-to-many"))["errors"] == []


def test_an_external_reference_needs_no_inverse(senzor):
    """DM-69: `refDevice` has no range, as Smart Data Models writes it, and still renders."""
    prop = compile_schema(senzor)["definitions"]["AirQualityObserved"]["properties"]["refDevice"]
    assert prop["x-ngsi-ld-kind"] == "Relationship"
    assert "x-ngsi-ld-relationship" not in prop


def _conforms(source: str, *entities: dict) -> tuple[bool, str]:
    context = compile_context(source)["@context"]
    data = Graph()
    for entity in entities:
        payload = {"@context": [{"id": "@id", "type": "@type"}, context], **entity}
        data.parse(data=json.dumps(payload), format="json-ld")
    shapes = Graph().parse(data=compile_shacl(source), format="turtle")
    conforms, _, text = validate(data, shacl_graph=shapes, advanced=True)
    return conforms, text


def _school(**extra) -> dict:
    return {"id": SCHOOL, "type": "School", "name": "Gymnázium", **extra}


def _user(**extra) -> dict:
    return {"id": USER, "type": "User", "name": "Jana", **extra}


def test_the_shapes_accept_a_user_of_one_school_and_refuse_everything_else():
    """DM-72 in a consumer's validator: `sh:class`, IRI, `sh:maxCount 1`, and the closed shape
    refusing the computed end on the School (DM-67)."""
    source = model("one-to-many", lambda doc: doc["slots"]["school"].update(required=True))
    assert _conforms(source, _school(), _user(school=SCHOOL))[0]

    conforms, report = _conforms(source, _school(), _user())
    assert not conforms and "MinCount" in report
    other = {"id": OTHER_SCHOOL, "type": "School", "name": "ZŠ"}
    conforms, report = _conforms(source, _school(), other, _user(school=[SCHOOL, OTHER_SCHOOL]))
    assert not conforms and "MaxCount" in report
    conforms, report = _conforms(source, _user(school=USER), _school())
    assert not conforms and "ClassConstraint" in report
    conforms, report = _conforms(source, _school(users=[USER]), _user(school=SCHOOL))
    assert not conforms and "Closed" in report


def test_the_owl_pair_is_inverse_and_a_single_end_is_functional():
    graph = Graph().parse(data=compile_owl(model("one-to-many")), format="turtle")
    users, school = URIRef("https://rozvoj.sk/terms/users"), URIRef("https://rozvoj.sk/terms/school")
    assert (users, OWL.inverseOf, school) in graph
    assert (school, RDF.type, OWL.FunctionalProperty) in graph
    assert (users, RDF.type, OWL.FunctionalProperty) not in graph

    many = Graph().parse(data=compile_owl(model("many-to-many")), format="turtle")
    assert not list(many.subjects(RDF.type, OWL.FunctionalProperty))


def test_the_context_expands_both_ends_as_iris():
    context = compile_context(model("many-to-many"))["@context"]
    assert context["users"]["@type"] == "@id"
    assert context["school"]["@type"] == "@id"
