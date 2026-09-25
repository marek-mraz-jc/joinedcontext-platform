"""The first data cube, population by district, age band and year (T-1186, T-1188, DM-60)."""

from __future__ import annotations

import copy
from pathlib import Path

import pytest
import yaml
from rdflib import Graph, Namespace, URIRef
from rdflib.namespace import RDF, RDFS

from common import ModelError
from gen_json_schema import compile_schema
from gen_qb import compile_qb
from service import artifacts

FIXTURES = Path(__file__).parent / "fixtures"
CUBE = FIXTURES / "population-cube.linkml.yaml"
GOLDEN = FIXTURES / "population-cube.qb.ttl"
QB = Namespace("http://purl.org/linked-data/cube#")
BB = Namespace("https://banskabystrica.sk/ns/")


def cube() -> dict:
    return yaml.safe_load(CUBE.read_text(encoding="utf-8"))


def text(document: dict) -> str:
    return yaml.safe_dump(document, sort_keys=False, allow_unicode=True)


def test_qb_dsd_generated_from_model():
    """The Turtle is the reviewed golden file, byte for byte, and it parses (DM-44)."""
    rendered = compile_qb(str(CUBE))
    assert rendered == GOLDEN.read_text(encoding="utf-8")
    graph = Graph().parse(data=rendered, format="turtle")
    dsd = BB.PopulationObservationStructure
    assert (dsd, RDF.type, QB.DataStructureDefinition) in graph
    components = {
        (graph.value(node, QB.dimension) or graph.value(node, QB.measure), int(graph.value(node, QB.order)))
        for node in graph.objects(dsd, QB.component)
    }
    assert components == {(BB.refDistrict, 1), (BB.ageBand, 2), (BB.year, 3), (BB.population, 4)}
    assert BB.source not in {c for c, _ in components}, "a slot without a role is no component"


def test_qb_observation_shape_generated():
    """The class is what its entities are, and each component is typed by its role."""
    graph = Graph().parse(data=compile_qb(str(CUBE)), format="turtle")
    assert (BB.PopulationObservation, RDFS.subClassOf, QB.Observation) in graph
    for dimension in (BB.refDistrict, BB.ageBand, BB.year):
        assert (dimension, RDF.type, QB.DimensionProperty) in graph
    assert (BB.population, RDF.type, QB.MeasureProperty) in graph
    assert (BB.year, RDFS.range, URIRef("http://www.w3.org/2001/XMLSchema#integer")) in graph


def test_a_model_without_a_dsd_renders_no_cube(senzor):
    assert compile_qb(senzor) is None
    answer = artifacts(Path(senzor).read_text(encoding="utf-8"))
    assert "qb" not in answer and answer["errors"] == []
    assert artifacts(CUBE.read_text(encoding="utf-8"))["qb"] == GOLDEN.read_text(encoding="utf-8")


def test_the_json_schema_carries_the_roles_the_gateway_reads():
    schema = compile_schema(str(CUBE))
    observation = schema["definitions"]["PopulationObservation"]
    assert observation["x-qb-dsd"] is True
    roles = {name: prop.get("x-qb-component") for name, prop in observation["properties"].items()}
    assert roles["refDistrict"] == roles["ageBand"] == roles["year"] == "dimension"
    assert roles["population"] == "measure"
    assert roles["source"] is None and roles["id"] is None


def test_a_dimension_that_is_a_relationship_to_a_class_keeps_its_role():
    """The relationship pass rebuilds the slot; the role survives it."""
    document = cube()
    document["classes"]["District"] = {
        "class_uri": "bb:District",
        "is_a": "Entity",
        "slots": ["observations"],
    }
    document["slots"]["refDistrict"]["range"] = "District"
    document["slots"]["refDistrict"]["inverse"] = "observations"
    document["slots"]["observations"] = {
        "slot_uri": "bb:observations",
        "range": "PopulationObservation",
        "multivalued": True,
        "inverse": "refDistrict",
        "annotations": {"ngsi_ld_kind": "Relationship"},
    }
    schema = compile_schema(text(document))
    district = schema["definitions"]["PopulationObservation"]["properties"]["refDistrict"]
    assert district["x-qb-component"] == "dimension"
    assert district["x-ngsi-ld-relationship"]["target"] == "District"
    assert f"rdfs:range <{BB.District}>" in compile_qb(text(document))


def broken(edit) -> str:
    document = copy.deepcopy(cube())
    edit(document)
    return text(document)


def _set(path: list[str], value):
    def edit(document):
        node = document
        for key in path[:-1]:
            node = node.setdefault(key, {})
        if value is None:
            node.pop(path[-1], None)
        else:
            node[path[-1]] = value

    return edit


@pytest.mark.parametrize(
    ("edit", "said"),
    [
        (_set(["slots", "ageBand", "required"], None), "slot 'ageBand' of the DSD 'PopulationObservation' is a dimension and must be required"),
        (_set(["slots", "population", "unit"], None), "slot 'population' of the DSD 'PopulationObservation' is a measure without a unit"),
        (_set(["slots", "ageBand", "annotations", "ngsi_ld_kind"], "Property"), "is a dimension of kind Property with range AgeBand"),
        (_set(["slots", "population", "annotations", "ngsi_ld_kind"], "VocabProperty"), "is a measure of kind VocabProperty"),
        (_set(["slots", "ageBand", "multivalued"], True), "cannot be multivalued"),
        (_set(["slots", "population", "annotations", "qb_component"], "measures"), "has qb_component 'measures'"),
        (_set(["slots", "population", "annotations", "qb_component"], None), "needs at least one dimension and one measure"),
        (_set(["classes", "PopulationObservation", "annotations", "qb_dsd"], "yes"), "has qb_dsd 'yes'"),
        (_set(["classes", "PopulationObservation", "annotations", "qb_dsd"], None), "slot 'population' declares qb_component but no class using it"),
    ],
)
def test_a_dsd_that_breaks_a_rule_is_refused_naming_the_slot(edit, said):
    source = broken(edit)
    with pytest.raises(ModelError, match="the Data Structure Definition is not a cube") as refused:
        compile_schema(source)
    assert said in str(refused.value)
    # The editor reads the same sentence from `/generate`, once for every generator.
    assert any(said in message for message in artifacts(source)["errors"])


def test_two_time_dimensions_are_refused():
    def edit(document):
        document["slots"]["month"] = {
            "slot_uri": "bb:month",
            "range": "date",
            "required": True,
            "annotations": {"ngsi_ld_kind": "Property", "qb_component": "dimension"},
        }
        document["classes"]["PopulationObservation"]["slots"].append("month")

    with pytest.raises(ModelError, match="has 2 time dimensions"):
        compile_qb(broken(edit))
