"""The generated valid and invalid datasets, and SHACL and the JSON Schema agreeing on them (T-5066).

Each model's valid records must pass the shapes and the schema, each invalid record must fail
both, and SHACL must name the slot the record breaks. A checker that cannot go red proves
nothing, so one test hands it the shapes LinkML renders before the NGSI-LD fixes and expects
the disagreements back.
"""

from __future__ import annotations

import json
import re

import pytest
from conftest import FIXTURES
from linkml.generators.shaclgen import ShaclGenerator

import gen_datasets
from common import load
from gen_datasets import Checker, breaker, check, compile_datasets, main, sample

CONSTRAINTS = str(FIXTURES / "constraints.linkml.yaml")


@pytest.mark.parametrize("model", ["senzor.linkml.yaml", "constraints.linkml.yaml"])
def test_shacl_and_the_json_schema_agree_on_every_record(model):
    assert check(str(FIXTURES / model)) == []


def test_the_valid_dataset_reaches_every_edge():
    sets = compile_datasets(CONSTRAINTS)
    stations = [r for r in sets.valid if r["type"] == "Station"]
    assert {r.get("capacity") for r in stations} >= {1, 40}
    assert {r.get("temperature") for r in stations} >= {-40.0}
    assert {r.get("level") for r in stations} >= {"low", "medium", "high"}
    assert {r["location"]["type"] for r in stations if "location" in r} == {
        "Point", "MultiPoint", "LineString", "MultiLineString", "Polygon", "MultiPolygon",
    }
    assert any(set(r["name"]) == {"en", "sk"} for r in stations)
    # Each optional slot is left out of one record; the required one never is.
    assert any("code" not in r for r in stations)
    assert all("name" in r for r in sets.valid)
    # Every pattern value matches, read off the pattern itself.
    assert all(re.search("^[A-Z]{2}[0-9]{3}$", r["code"]) for r in stations if "code" in r)
    assert {r["type"] for r in sets.valid} == {"Station", "Operator"}


def test_the_invalid_dataset_breaks_one_constraint_each():
    broken = {(i.record["type"], i.path, i.constraint) for i in compile_datasets(CONSTRAINTS).invalid}
    assert broken >= {
        ("Station", "name", "required"),
        ("Station", "capacity", "minimum"),
        ("Station", "capacity", "maximum"),
        ("Station", "temperature", "minimum"),
        ("Station", "capacity", "datatype"),
        ("Station", "code", "pattern"),
        ("Station", "level", "enum"),
        ("Station", "undeclaredAttribute", "closed"),
    }
    # No upper bound declared, so none is broken.
    assert ("Station", "temperature", "maximum") not in broken


def test_a_list_member_of_the_wrong_type_is_refused_at_its_slot():
    record = dict(compile_datasets(CONSTRAINTS).valid[0], depths=["deep"])
    verdict = Checker(CONSTRAINTS).verdict(record)
    assert not verdict.shacl_ok
    assert verdict.shacl_paths == ["depths"]
    assert not verdict.schema_ok


def test_the_checker_goes_red_on_shapes_that_disagree(monkeypatch):
    # The shapes as LinkML renders them, before compile_shacl makes them agree with the context.
    raw = ShaclGenerator(load(CONSTRAINTS).schema, closed=True).serialize()
    monkeypatch.setattr(gen_datasets, "compile_shacl", lambda _source: raw)
    problems = check(CONSTRAINTS)
    assert any("valid Station refused by SHACL" in p and "address" in p for p in problems), problems
    assert any("depths" in p for p in problems), problems


def test_the_cli_writes_both_datasets_and_exits_0(tmp_path, capsys):
    assert main([CONSTRAINTS, "-o", str(tmp_path)]) == 0
    assert "0 disagreements" in capsys.readouterr().out
    valid = json.loads((tmp_path / "constraints.valid.json").read_text())
    invalid = json.loads((tmp_path / "constraints.invalid.json").read_text())
    assert len(valid) == len(compile_datasets(CONSTRAINTS).valid)
    assert {"path", "constraint", "record"} <= set(invalid[0])


def test_a_model_without_an_entity_class_is_an_error(tmp_path, capsys):
    model = tmp_path / "values.linkml.yaml"
    model.write_text(
        "id: https://banskabystrica.sk/ns/values\nname: values\nimports: [linkml:types]\n"
        "prefixes: { linkml: https://w3id.org/linkml/ }\nclasses: { Note: { attributes: { text: {} } } }\n"
    )
    assert main([str(model)]) == 1
    assert "nothing to test" in capsys.readouterr().err


@pytest.mark.parametrize(
    "pattern",
    ["^[012]$", "^[A-Z]{2}[0-9]{3}$", r"^\d{4}-\d{2}$", "^(abc|d)x+$", "^[^<>]{0,500}$", "^urn:ngsi-ld:[A-Za-z]+:.+$"],
)
def test_sample_finds_a_value_and_breaker_one_it_refuses(pattern):
    found = sample(pattern)
    assert found is not None and re.search(pattern, found)
    refused = breaker(pattern)
    assert refused is not None and not re.search(pattern, refused)


def test_sample_gives_up_on_what_it_cannot_spell():
    assert sample(r"^(?=a)b$") is None
    assert breaker(".*") is None


@pytest.mark.parametrize("model", ["senzor", "constraints"])
def test_the_committed_write_path_fixtures_are_what_the_generator_renders(model, tmp_path):
    # crates/jc-manifest/tests/model_datasets_tests.rs reads these without Model Tools installed.
    committed = FIXTURES.parents[3] / "crates/jc-manifest/tests/fixtures/model-datasets"
    assert main([str(FIXTURES / f"{model}.linkml.yaml"), "-o", str(tmp_path)]) == 0
    for kind in ("valid", "invalid", "schema"):
        name = f"{model}.{kind}.json"
        assert (tmp_path / name).read_text() == (committed / name).read_text(), (
            f"regenerate {name}: python3 src/gen_datasets.py tests/fixtures/{model}.linkml.yaml -o {committed}"
        )


def test_a_model_mapping_schema_to_https_keeps_its_own_iris(tmp_path):
    # LinkML maps `schema:` to http://schema.org/ by default; a model that maps it to https kept
    # its own IRIs in the @context and the shapes, and the shape fixes looked for the http ones,
    # so every valid event with an inline address was refused at `address` (T-5066).
    model = tmp_path / "events.linkml.yaml"
    model.write_text(
        "id: https://banskabystrica.sk/ns/events\nname: events\n"
        "prefixes: { linkml: https://w3id.org/linkml/, schema: https://schema.org/ }\n"
        "default_range: string\nimports: [linkml:types, ngsi-ld-core]\n"
        "classes:\n"
        "  Event: { class_uri: schema:Event, is_a: Entity, slots: [address] }\n"
        "  PostalAddress: { class_uri: schema:PostalAddress, slots: [addressLocality] }\n"
        "slots:\n"
        "  address: { slot_uri: schema:address, range: PostalAddress, inlined: true, required: true,"
        " annotations: { ngsi_ld_kind: Property } }\n"
        "  addressLocality: { slot_uri: schema:addressLocality, annotations: { ngsi_ld_kind: Property } }\n"
    )
    assert check(str(model)) == []
