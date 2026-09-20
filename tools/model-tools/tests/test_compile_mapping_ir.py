"""The gateway mapping IR (T-0173, DM-51, DM-52, EP-54).

The IR is not a second compiler for the same job. Bloblang translates a message; the IR is
what the Context Gateway executes when it proxies a live query, and a live query has to travel
*backwards* through the mapping: `attrs`, `q` and the enum values inside them are written in
the target model and reach the context source in the source's own names. Only an invertible
derivation has a backwards direction, so the tests here are about which derivations get one,
which are served without one, and which are refused outright.

The numbers themselves are not re-derived here. Both compilers call the same
`linear_conversion`, and the test that says so is the one asserting the IR factor equals the
factor in the compiled Bloblang.
"""

from __future__ import annotations

import json

import pytest

from common import ModelError
from compile_bloblang import _bloblang, compile_bloblang
from compile_mapping_ir import IR_VERSION, compile_mapping_ir
from test_compile_bloblang import derive


def slots(ir: dict) -> dict:
    return {slot["target"]: slot for slot in ir["slots"]}


# --- the document ---------------------------------------------------------------------


def test_the_ir_names_its_version_and_both_classes(transformation, mapping_source):
    ir = compile_mapping_ir(transformation, source=mapping_source)
    assert ir["version"] == IR_VERSION
    assert ir["sourceClass"] == "AirQualityObserved"
    assert ir["targetClass"] == "AirQuality"
    # The gateway reads artifacts a repository committed under an older Model Tools (DM-52),
    # so the whole document has to survive a round trip through JSON.
    assert json.loads(json.dumps(ir)) == ir


def test_every_derivation_of_the_golden_mapping_has_a_kind(transformation, mapping_source):
    kinds = {name: slot["kind"] for name, slot in
             slots(compile_mapping_ir(transformation, source=mapping_source)).items()}
    assert kinds == {
        "id": "rename", "pm25": "rename", "label": "expr", "band": "valueMappings",
        "readings": "cast", "degrees": "cast", "distanceCm": "unitConversion",
    }


def test_a_rename_carries_the_source_slot_and_is_filterable(mapping_source):
    ir = compile_mapping_ir(derive({"pm25": {"populated_from": "pm2p5"}}), source=mapping_source)
    assert slots(ir)["pm25"] == {
        "target": "pm25", "source": "pm2p5", "kind": "rename", "filterable": True
    }


# --- what the gateway can invert ------------------------------------------------------


def test_value_mappings_carry_both_directions(mapping_source):
    ir = compile_mapping_ir(
        derive({"band": {"populated_from": "airQualityLevel",
                         "value_mappings": {"good": "A", "moderate": "B"}}}),
        source=mapping_source,
    )
    band = slots(ir)["band"]
    assert band["forward"] == {"good": "A", "moderate": "B"}
    # `q=band=="A"` reaches the context source as `airQualityLevel=="good"`, which is the
    # whole reason the inverse is precomputed rather than derived at request time (EP-54).
    assert band["inverse"] == {"A": "good", "B": "moderate"}
    assert band["filterable"] is True


def test_a_unit_conversion_is_a_factor_and_an_offset(mapping_source):
    ir = compile_mapping_ir(
        derive({"distanceCm": {"populated_from": "distanceToRoad",
                               "unit_conversion": {"target_unit": "cm"}}}),
        source=mapping_source,
    )
    assert slots(ir)["distanceCm"] == {
        "target": "distanceCm", "source": "distanceToRoad", "kind": "unitConversion",
        "factor": 100.0, "offset": 0.0, "filterable": True,
    }


def test_the_ir_factor_is_the_factor_in_the_compiled_bloblang(transformation, mapping_source):
    """One conversion, two artifacts, one number: DM-52 is only true if they agree."""
    ir = compile_mapping_ir(transformation, source=mapping_source)
    artifact = compile_bloblang(transformation, source=mapping_source)
    factor = slots(ir)["distanceCm"]["factor"]
    line = next(row["bloblang"] for row in artifact.report if row["targetSlot"] == "distanceCm")
    assert f"* {factor!r}" in line


def test_a_cast_names_the_range_it_casts_to(mapping_source):
    ir = compile_mapping_ir(
        derive({"readings": {"populated_from": "readingCount", "range": "integer"}}),
        source=mapping_source,
    )
    assert slots(ir)["readings"]["range"] == "integer"
    assert slots(ir)["readings"]["filterable"] is True


# --- what the gateway serves but cannot filter ----------------------------------------


def test_an_expression_is_served_and_not_filterable(mapping_source):
    ir = compile_mapping_ir(derive({"label": {"expr": '{stationName} + "!"'}}),
                            source=mapping_source)
    label = slots(ir)["label"]
    assert label["kind"] == "expr"
    # DM-51: the gateway returns the computed value and refuses to translate a filter naming
    # it, because there is no source slot a filter could be rewritten onto.
    assert label["filterable"] is False
    assert "source" not in label
    # The other half of DM-51. Without the tree the gateway has nothing to compute and the
    # slot is absent from the answer, which a consumer cannot tell from an entity that has no
    # value for it.
    assert label["expression"] == {
        "binary": "+", "left": {"slot": "stationName"}, "right": {"const": "!"}
    }


def test_the_expression_tree_covers_every_form_the_compiler_accepts(mapping_source):
    ir = compile_mapping_ir(
        derive({"flag": {"expr": "not ({pm2p5} > 10 and -{pm2p5} / 2 < 1) or {ok}"}}),
        source=mapping_source,
    )
    assert slots(ir)["flag"]["expression"] == {
        "boolean": "or",
        "operands": [
            {"unary": "not", "operand": {
                "boolean": "and",
                "operands": [
                    {"compare": ">", "left": {"slot": "pm2p5"}, "right": {"const": 10}},
                    {"compare": "<",
                     "left": {"binary": "/",
                              "left": {"unary": "-", "operand": {"slot": "pm2p5"}},
                              "right": {"const": 2}},
                     "right": {"const": 1}},
                ],
            }},
            {"slot": "ok"},
        ],
    }


@pytest.mark.parametrize("expression", [
    '{stationName} + " (" + {areaServed} + ")"',
    "{pm2p5} * 2 - 1",
    "not ({pm2p5} > 10 and -{pm2p5} / 2 < 1) or {ok}",
    '{stationName} == "x" or {pm2p5} != 3',
])
def test_the_tree_in_the_ir_renders_the_bloblang_bento_runs(expression, mapping_source):
    """DM-52's guarantee, as far as it can be checked without Bento.

    One specification, two artifacts, and the IR carries the tree the Bloblang was rendered
    from. If the two ever came from different parses this would be the first thing to fail.
    """
    mapping = derive({"label": {"expr": expression}})
    ir = compile_mapping_ir(mapping, source=mapping_source)
    rendered = _bloblang(slots(ir)["label"]["expression"])
    line = next(row["bloblang"] for row in compile_bloblang(mapping, source=mapping_source).report
                if row["targetSlot"] == "label")
    assert line == f"root.label = {rendered}"


def test_a_constant_is_served_and_not_filterable(mapping_source):
    ir = compile_mapping_ir(derive({"origin": {"value": "sdm"}}), source=mapping_source)
    assert slots(ir)["origin"] == {
        "target": "origin", "kind": "constant", "value": "sdm", "filterable": False
    }


def test_a_hidden_slot_is_absent_from_the_ir(mapping_source):
    ir = compile_mapping_ir(derive({"id": {}, "temperature": {"hide": True}}),
                            source=mapping_source)
    assert "temperature" not in slots(ir)


# --- what is refused ------------------------------------------------------------------


def test_a_non_invertible_value_mapping_is_refused(mapping_source):
    """Two source values on one target value: a query for it has no single answer."""
    with pytest.raises(ModelError, match="no single source"):
        compile_mapping_ir(
            derive({"band": {"populated_from": "airQualityLevel",
                             "value_mappings": {"good": "A", "moderate": "A"}}}),
            source=mapping_source,
        )


def test_a_native_block_is_refused_outright(mapping_source):
    # A native block has no inverse and nothing to inspect, so live translation through it is
    # a guess. Replicate mode is where such a Mapping belongs (DM-51).
    with pytest.raises(ModelError, match="replicate mode"):
        compile_mapping_ir(derive({"id": {}}), source=mapping_source,
                           native=[{"targetSlot": "id", "source": "root.id = 1"}])


def test_a_boolean_cast_is_refused_here_too(mapping_source):
    with pytest.raises(ModelError, match="boolean"):
        compile_mapping_ir(derive({"active": {"populated_from": "stationName",
                                              "range": "boolean"}}), source=mapping_source)


def test_a_populated_from_naming_another_table_is_refused(mapping_source):
    with pytest.raises(ModelError, match="nothing to join against"):
        compile_mapping_ir(derive({"name": {"populated_from": "Station.name"}}),
                           source=mapping_source)


def test_joins_are_refused_here_too(mapping_source):
    spec = derive({"id": {}})
    spec["class_derivations"]["AirQuality"]["joins"] = {
        "station": {"class_named": "Station", "join_on": "stationName"}
    }
    with pytest.raises(ModelError, match="joins"):
        compile_mapping_ir(spec, source=mapping_source)
