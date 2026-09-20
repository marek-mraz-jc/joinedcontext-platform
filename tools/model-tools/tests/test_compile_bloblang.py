"""The LinkML-Map to Bloblang compiler (T-0172, DM-33, DM-35, DM-36, DM-37).

Two properties are worth testing here and nothing else is. First, that every derivation
DM-36 allows compiles to Bloblang Bento accepts: a mapping that renders but does not parse is
a pipeline that fails on the first message. Second, that everything DM-36 does *not* allow is
refused with a message naming the derivation, because the alternative to a refusal is a
mapping that runs and quietly produces a wrong entity.

Whether the compiled Bloblang means the same as the specification is not tested here. That
is parity, it needs both engines, and `test_parity_runner` is where it lives.
"""

from __future__ import annotations

from copy import deepcopy

import pytest

from common import ModelError
from compile_bloblang import compile_bloblang
from conftest import bloblang, requires_bento


def derive(slots: dict, populated_from: str = "AirQualityObserved") -> dict:
    """A transformation deriving one class with the given slot derivations."""
    return {
        "id": "https://example.org/ns/test/transform",
        "class_derivations": {
            "AirQuality": {"populated_from": populated_from, "slot_derivations": slots}
        },
    }


def line(artifact, target: str) -> str:
    """The compiled line of one target slot, from the compile report (DM-35)."""
    rows = [row for row in artifact.report if row["targetSlot"] == target]
    assert rows, f"no compile report row for '{target}'"
    return rows[0]["bloblang"]


# --- what compiles --------------------------------------------------------------------


def test_rename_reads_the_source_slot(mapping_source):
    artifact = compile_bloblang(derive({"pm25": {"populated_from": "pm2p5"}}),
                                source=mapping_source)
    assert line(artifact, "pm25") == "root.pm25 = this.pm2p5"
    assert artifact.source_class == "AirQualityObserved"
    assert artifact.target_class == "AirQuality"


def test_a_slot_without_populated_from_reads_its_own_name(mapping_source):
    artifact = compile_bloblang(derive({"id": {}}), source=mapping_source)
    assert line(artifact, "id") == "root.id = this.id"


def test_populated_from_qualified_by_the_source_class_is_the_same_read(mapping_source):
    artifact = compile_bloblang(
        derive({"pm25": {"populated_from": "AirQualityObserved.pm2p5"}}), source=mapping_source
    )
    assert line(artifact, "pm25") == "root.pm25 = this.pm2p5"


def test_value_mappings_become_a_match_with_a_throwing_default(mapping_source):
    artifact = compile_bloblang(
        derive({"band": {"populated_from": "airQualityLevel",
                         "value_mappings": {"good": "A", "moderate": "B"}}}),
        source=mapping_source,
    )
    compiled = line(artifact, "band")
    assert "match this.airQualityLevel {" in compiled
    assert '"good" => "A"' in compiled
    assert '"moderate" => "B"' in compiled
    # DM-36 forbids a silent fallback: an unmapped enum value is a data error, not a pass-through.
    assert "_ => throw(" in compiled


def test_unit_conversion_becomes_arithmetic_on_the_declared_unit(mapping_source):
    artifact = compile_bloblang(
        derive({"distanceCm": {"populated_from": "distanceToRoad",
                               "unit_conversion": {"target_unit": "cm"}}}),
        source=mapping_source,
    )
    # The source unit comes from the model (metres, DM-06) and neither Bloblang nor the
    # gateway has a unit registry, so the conversion has to reduce to a factor.
    assert line(artifact, "distanceCm") == "root.distanceCm = (this.distanceToRoad.number() * 100.0)"


def test_unit_conversion_may_name_its_own_source_unit_without_a_schema():
    artifact = compile_bloblang(
        derive({"distanceCm": {"populated_from": "distanceToRoad",
                               "unit_conversion": {"source_unit": "km", "target_unit": "m"}}})
    )
    assert line(artifact, "distanceCm") == "root.distanceCm = (this.distanceToRoad.number() * 1000.0)"


def test_expr_compiles_arithmetic_and_concatenation(mapping_source):
    artifact = compile_bloblang(
        derive({"label": {"expr": '{stationName} + " (" + {areaServed} + ")"'},
                "doubled": {"expr": "{pm2p5} * 2"}}),
        source=mapping_source,
    )
    assert line(artifact, "label") == (
        'root.label = (((this.stationName + " (") + this.areaServed) + ")")'
    )
    assert line(artifact, "doubled") == "root.doubled = (this.pm2p5 * 2)"


def test_a_constant_value_needs_no_source_slot(mapping_source):
    artifact = compile_bloblang(derive({"source": {"value": "sdm"}}), source=mapping_source)
    assert line(artifact, "source") == 'root.source = "sdm"'


def test_hide_and_derived_from_produce_no_line(mapping_source):
    artifact = compile_bloblang(
        derive({"id": {}, "temperature": {"hide": True},
                "provenance": {"derived_from": ["stationName"]}}),
        source=mapping_source,
    )
    assert line(artifact, "temperature") is None
    assert line(artifact, "provenance") is None
    assert "temperature" not in artifact.mapping
    assert "provenance" not in artifact.mapping


def test_a_native_block_is_copied_through_and_reported_unchecked(mapping_source):
    artifact = compile_bloblang(
        derive({"id": {}, "odd": {"populated_from": "stationName"}}),
        source=mapping_source,
        native=[{"targetSlot": "odd", "language": "bloblang",
                 "source": 'root.odd = this.stationName.uppercase()'}],
    )
    assert 'root.odd = this.stationName.uppercase()' in artifact.mapping
    rows = {row["targetSlot"]: row for row in artifact.report}
    assert rows["odd"]["derivation"] == "native"
    # DM-38: the schema guarantee stops at a native block, and the report is what says so.
    assert rows["odd"]["checked"] is False
    assert rows["id"]["checked"] is True


# --- the integer cast -----------------------------------------------------------------


def test_integer_cast_matches_python_int_on_both_shapes(mapping_source):
    """`int()` truncates a number towards zero and refuses a non-integer string (DM-39)."""
    artifact = compile_bloblang(
        derive({"readings": {"populated_from": "readingCount", "range": "integer"}}),
        source=mapping_source,
    )
    compiled = line(artifact, "readings")
    assert ".ceil()" in compiled and ".floor() }" in compiled
    assert 're_match("^[+-]?[0-9]+$")' in compiled


@requires_bento
def test_the_integer_cast_runs_the_way_python_int_does(mapping_source):
    artifact = compile_bloblang(
        derive({"readings": {"populated_from": "readingCount", "range": "integer"},
                "degrees": {"populated_from": "temperature", "range": "integer"}}),
        source=mapping_source,
    )
    produced = bloblang(artifact.mapping, {"readingCount": "-4", "temperature": -3.5})
    assert produced == {"readings": -4, "degrees": -3}
    # A number truncates towards zero rather than rounding down, which `floor()` alone does not.
    assert bloblang(artifact.mapping, {"readingCount": "7", "temperature": 3.9})["degrees"] == 3


@requires_bento
def test_an_integer_cast_of_a_decimal_string_throws_rather_than_truncating(mapping_source):
    artifact = compile_bloblang(
        derive({"readings": {"populated_from": "readingCount", "range": "integer"}}),
        source=mapping_source,
    )
    # `int("-4.7")` raises in the reference engine, so the compiled mapping must fail too.
    # Truncating here is the silent wrong answer DM-36 exists to prevent.
    with pytest.raises(RuntimeError, match="not an integer"):
        bloblang(artifact.mapping, {"readingCount": "-4.7"})


def test_string_and_float_casts_compile(mapping_source):
    artifact = compile_bloblang(
        derive({"stationName": {"range": "string"}, "pm25": {"populated_from": "pm2p5",
                                                             "range": "float"}}),
        source=mapping_source,
    )
    assert line(artifact, "stationName") == "root.stationName = this.stationName.string()"
    assert line(artifact, "pm25") == "root.pm25 = this.pm2p5.number()"


# --- the whole mapping ----------------------------------------------------------------


@requires_bento
def test_the_golden_mapping_is_bloblang_bento_parses(transformation, mapping_source):
    artifact = compile_bloblang(transformation, source=mapping_source)
    produced = bloblang(artifact.mapping, {
        "id": "urn:x", "pm2p5": 12.5, "temperature": 21.0, "distanceToRoad": 2.5,
        "stationName": "Sever", "areaServed": "Zvolen", "airQualityLevel": "good",
        "readingCount": "7",
    })
    assert produced == {
        "id": "urn:x", "pm25": 12.5, "label": "Sever (Zvolen)", "band": "A",
        "readings": 7, "degrees": 21, "distanceCm": 250,
    }


def test_the_report_carries_one_row_per_derivation(transformation, mapping_source):
    artifact = compile_bloblang(transformation, source=mapping_source)
    kinds = {row["targetSlot"]: row["derivation"] for row in artifact.report}
    assert kinds == {
        "id": "populated_from", "pm25": "populated_from", "label": "expr",
        "band": "value_mappings", "readings": "populated_from", "degrees": "populated_from",
        "distanceCm": "unit_conversion",
    }
    assert all(row["checked"] for row in artifact.report)


# --- what is refused ------------------------------------------------------------------


def test_joins_are_refused_because_a_message_has_nothing_to_join_against(mapping_source):
    spec = derive({"id": {}})
    spec["class_derivations"]["AirQuality"]["joins"] = {
        "station": {"class_named": "Station", "join_on": "stationName"}
    }
    with pytest.raises(ModelError, match="joins"):
        compile_bloblang(spec, source=mapping_source)


def test_two_target_classes_are_refused(mapping_source):
    spec = derive({"id": {}})
    spec["class_derivations"]["Other"] = {
        "populated_from": "AirQualityObserved", "slot_derivations": {"id": {}}
    }
    with pytest.raises(ModelError, match="produces one target class"):
        compile_bloblang(spec, source=mapping_source)


def test_a_boolean_cast_is_refused(mapping_source):
    # Python casts by truthiness and Bloblang parses the text, so `bool("false")` disagrees.
    with pytest.raises(ModelError, match="boolean"):
        compile_bloblang(derive({"active": {"populated_from": "stationName",
                                            "range": "boolean"}}), source=mapping_source)


@pytest.mark.parametrize("expression", [
    'len({stationName})',
    '{stationName}[0]',
    '[x for x in {stationName}]',
    '{stationName}.upper()',
    '__import__("os").system("id")',
])
def test_an_expr_that_is_not_arithmetic_is_refused(expression, mapping_source):
    """Refusal is by AST node type, so it holds for code nobody thought to blocklist."""
    with pytest.raises(ModelError, match="target slot 'label'"):
        compile_bloblang(derive({"label": {"expr": expression}}), source=mapping_source)


def test_a_chained_comparison_is_refused(mapping_source):
    with pytest.raises(ModelError, match="chained comparison"):
        compile_bloblang(derive({"ok": {"expr": "1 < {pm2p5} < 5"}}), source=mapping_source)


def test_a_populated_from_naming_another_table_is_refused(mapping_source):
    with pytest.raises(ModelError, match="lookup across messages"):
        compile_bloblang(derive({"name": {"populated_from": "Station.name"}}),
                         source=mapping_source)


def test_a_unit_conversion_with_no_declared_unit_is_refused(mapping_source):
    with pytest.raises(ModelError, match="declares no unit"):
        compile_bloblang(
            derive({"nameCm": {"populated_from": "stationName",
                               "unit_conversion": {"target_unit": "cm"}}}),
            source=mapping_source,
        )


def test_a_non_linear_unit_conversion_is_refused(mapping_source):
    # Celsius to kelvin is an offset, not a factor; the registry refuses it outright and the
    # compiler has to say so rather than emitting a multiplication that is wrong everywhere.
    with pytest.raises(ModelError, match="distanceCm"):
        compile_bloblang(
            derive({"distanceCm": {"populated_from": "distanceToRoad",
                                   "unit_conversion": {"source_unit": "Cel",
                                                       "target_unit": "K"}}}),
            source=mapping_source,
        )


def test_a_native_block_for_no_slot_is_refused(mapping_source):
    with pytest.raises(ModelError, match="would never run"):
        compile_bloblang(derive({"id": {}}), source=mapping_source,
                         native=[{"targetSlot": "ghost", "source": "root.ghost = 1"}])


def test_a_native_block_in_another_language_is_refused(mapping_source):
    with pytest.raises(ModelError, match="only runtime language"):
        compile_bloblang(derive({"id": {}}), source=mapping_source,
                         native=[{"targetSlot": "id", "language": "python",
                                  "source": "root.id = 1"}])


def test_a_specification_that_is_not_a_document_is_refused():
    with pytest.raises(ModelError, match="TransformationSpecification"):
        compile_bloblang("class_derivations: everything")


def test_an_empty_class_derivation_is_refused(mapping_source):
    with pytest.raises(ModelError, match="no slot_derivations"):
        compile_bloblang(derive({}), source=mapping_source)


def test_the_manifest_transformation_is_not_mutated(transformation, mapping_source):
    """Normalization rewrites the document in place, so the compiler works on a copy."""
    before = deepcopy(transformation)
    compile_bloblang(transformation, source=mapping_source)
    assert transformation == before
