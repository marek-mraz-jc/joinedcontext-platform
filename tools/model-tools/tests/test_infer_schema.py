"""A draft model from a sample (T-0598, DM-54, DM-55): headers, ranges, units, geo, alignment,
the two forms agreeing, the cap, and the readers that resolve nothing."""

from __future__ import annotations

import base64
import io
import json
from pathlib import Path

import openpyxl
import pytest
import yaml

import infer_schema
import service
from infer_schema import SampleError, infer, parse
from test_service import call, no_network  # noqa: F401 - a fixture

SENSORS = (
    "id,name,temperature (°C),pm10_ugm3,observedAt,lat,lon,plate,active,parent,note\n"
    "1,Alpha,21.5,12,2026-09-01T10:00:00Z,48.7,19.1,BB123AB,true,urn:ngsi-ld:Station:hel.fi:s:1,ok\n"
    "2,Beta,19,15,2026-09-01T11:00:00Z,48.8,19.2,BB456CD,no,urn:ngsi-ld:Station:hel.fi:s:2,7\n"
).encode("utf-8")
INDEX = {
    "temperature": ["dataModel.Environment/AirQualityObserved"],
    "pm10": ["dataModel.Environment/AirQualityObserved"],
}


def slots_of(answer: dict) -> dict:
    return yaml.safe_load(answer["linkml"])["slots"]


def test_a_csv_becomes_one_class_with_typed_slots():
    """DM-54: one class from the stem, a slot per column, ranges from the values."""
    answer = infer("sensors.csv", SENSORS)
    model = yaml.safe_load(answer["linkml"])

    assert list(model["classes"]) == ["Sensors"]
    assert model["classes"]["Sensors"]["is_a"] == "Entity"
    assert model["imports"] == ["linkml:types", "ngsi-ld-core"]
    slots = model["slots"]
    assert slots["temperature"]["range"] == "float"
    assert slots["temperature"]["minimum_value"] == 19.0
    assert slots["temperature"]["maximum_value"] == 21.5
    assert slots["pm10"]["range"] == "integer"
    assert slots["pm10"]["minimum_value"] == 12
    assert slots["active"]["range"] == "boolean"
    assert slots["plate"]["pattern"] == "^[A-Z]{2}[0-9]{3}[A-Z]{2}$"
    assert slots["parent"] == {
        "range": "uriorcurie",
        "annotations": {"ngsi_ld_kind": "Relationship"},
    }
    assert answer["rows"] == 2
    assert answer["errors"] == []


def test_core_slots_and_a_coordinate_pair_are_recorded_and_not_redeclared():
    """`id`, `observedAt` and `location` come from Entity; a lat/lon pair is `location` (DM-54)."""
    answer = infer("sensors.csv", SENSORS)

    slots = slots_of(answer)
    assert not {"id", "observedAt", "location", "lat", "lon"} & set(slots)
    assert answer["detectedTypes"]["observedAt"] == "datetime"
    assert answer["detectedTypes"]["location"] == "GeoProperty"
    assert answer["detectedTypes"]["id"] == "integer"
    assert "lat" not in answer["detectedTypes"]


def test_a_header_becomes_a_name_and_keeps_its_text_as_the_title():
    """DM-55: the original header travels as `title`, text, never markup."""
    sample = b"<b>Temp</b> (\xc2\xb0C);2nd reading;temperature (\xc2\xb0C)\n1;2;3\n"
    answer = infer("t.csv", sample)

    slots = slots_of(answer)
    assert list(slots) == ["b_Temp_b", "_2nd_reading", "temperature"]
    assert slots["b_Temp_b"]["title"] == {"en": "<b>Temp</b> (°C)"}
    assert slots["b_Temp_b"]["unit"]["exact_mappings"] == ["ucefact:CEL", "qudt-unit:DEG_C"]
    titles = [op for op in answer["operations"] if op["op"] == "setTitle"]
    assert titles[0] == {"op": "setTitle", "target": "slot", "name": "b_Temp_b", "locale": "en", "value": "<b>Temp</b> (°C)"}
    assert all(infer_schema.NAME.match(name) for name in slots)


def test_units_come_from_the_header_as_cefact_codes():
    """DM-06: `pm10_ugm3` is GQ, `(°C)` is CEL, a bare letter is a unit only in brackets."""
    answer = infer("u.csv", b"pm10_ugm3,temperature (\xc2\xb0C),speed [km/h],height (m),speed m\n1,2,3,4,5\n")

    slots = slots_of(answer)
    # The UN/CEFACT code NGSI-LD puts on the wire and the QUDT anchor a federated reader
    # dereferences, side by side (DM-06, DM-59). `ug/m3` is `MassDensity` to QUDT, which is why
    # the table is read out of QUDT's own vocabulary rather than written from memory.
    assert slots["pm10"]["unit"] == {
        "ucum_code": "ug/m3",
        "exact_mappings": ["ucefact:GQ", "qudt-unit:MicroGM-PER-M3"],
        "has_quantity_kind": "qudt-quantkind:MassDensity",
    }
    assert slots["temperature"]["unit"]["exact_mappings"] == ["ucefact:CEL", "qudt-unit:DEG_C"]
    assert slots["speed"]["unit"]["exact_mappings"] == ["ucefact:KMH", "qudt-unit:KiloM-PER-HR"]
    assert slots["height"]["unit"]["exact_mappings"] == ["ucefact:MTR", "qudt-unit:M"]
    assert "unit" not in slots["speed_m"]
    units = [op for op in answer["operations"] if op["op"] == "setSlot" and op["field"] == "unit"]
    assert [op["value"] for op in units] == ["GQ", "CEL", "KMH", "MTR"]


def test_a_mixed_column_stays_a_string_and_is_named_under_untyped():
    answer = infer("sensors.csv", SENSORS)

    assert slots_of(answer)["note"] == {"range": "string"}
    assert answer["untyped"] == [{"slot": "note", "reason": "mixed values: 1 text, 1 integer"}]


def test_a_slot_the_catalogue_knows_binds_its_iri_with_the_citation():
    """DM-07: an attribute of the same name in Smart Data Models binds `slot_uri`, and the
    upstream citation keeps it out of the squatting guard (DM-04, DM-16). The operations leave
    the IRI out, because the editor refuses a reserved IRI through an operation."""
    answer = infer("sensors.csv", SENSORS, index=INDEX)

    slots = slots_of(answer)
    assert slots["pm10"]["slot_uri"] == "https://smartdatamodels.org/dataModel.Environment/pm10"
    assert slots["pm10"]["annotations"]["upstream_source"] == "dataModel.Environment/AirQualityObserved"
    assert "slot_uri" not in slots["name"]
    assert answer["matches"] == {
        "temperature": {
            "model": "dataModel.Environment/AirQualityObserved",
            "slotUri": "https://smartdatamodels.org/dataModel.Environment/temperature",
        },
        "pm10": {
            "model": "dataModel.Environment/AirQualityObserved",
            "slotUri": "https://smartdatamodels.org/dataModel.Environment/pm10",
        },
    }
    assert not any("slot_uri" in op for op in answer["operations"])


def test_the_operations_build_the_same_slots_as_the_source():
    """DM-13: the two forms say the same thing, so the editor applying the operations ends where
    the source is."""
    answer = infer("sensors.csv", SENSORS)

    slots = slots_of(answer)
    added = {op["name"]: op for op in answer["operations"] if op["op"] == "addSlot"}
    assert list(added) == list(slots)
    for name, definition in slots.items():
        assert added[name]["range"] == definition["range"]
        assert added[name]["class"] == "Sensors"
        kind = definition.get("annotations", {}).get("ngsi_ld_kind")
        assert added[name].get("kind") == kind
    assert answer["operations"][0] == {"op": "addClass", "name": "Sensors", "is_a": "Entity"}
    set_fields = {(op["name"], op["field"]): op["value"] for op in answer["operations"] if op["op"] == "setSlot"}
    assert set_fields[("plate", "pattern")] == slots["plate"]["pattern"]
    assert set_fields[("pm10", "maximum_value")] == 15


def test_the_inferred_model_compiles():
    """The draft is a model the generators accept, not only YAML the editor shows."""
    answer = infer("sensors.csv", SENSORS, index=INDEX)

    status, body = call("POST", "/generate", {"source": answer["linkml"]})

    assert status == 200
    assert body["errors"] == [], body["errors"]
    assert "pm10" in body["context"]["@context"]


def test_a_workbook_gives_one_class_per_sheet_with_typed_cells():
    book = openpyxl.Workbook()
    first = book.active
    first.title = "Stations"
    first.append(["id", "name", "capacity", "opened"])
    first.append(["s1", "North", 12, "2020-01-05"])
    first.append(["s2", "South", 8.5, "2021-03-09"])
    second = book.create_sheet("Readings")
    second.append(["station", "value", "when"])
    second.append(["s1", 3, "2026-09-01T10:00:00"])
    buffer = io.BytesIO()
    book.save(buffer)

    answer = infer("bikes.xlsx", buffer.getvalue())

    model = yaml.safe_load(answer["linkml"])
    assert list(model["classes"]) == ["Stations", "Readings"]
    assert model["classes"]["Stations"]["slots"] == ["name", "capacity", "opened"]
    assert model["slots"]["capacity"]["range"] == "float"
    assert model["slots"]["opened"]["range"] == "date"
    assert model["slots"]["when"]["range"] == "datetime"
    assert answer["rows"] == 3


def test_the_excel_reader_refuses_external_entities():
    """DM-55: openpyxl parses through defusedxml, which raises on an entity declaration instead
    of resolving it. The guard is asserted, not assumed."""
    import openpyxl.xml

    assert openpyxl.xml.DEFUSEDXML is True
    import zipfile

    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        archive.writestr(
            "[Content_Types].xml",
            '<?xml version="1.0"?><!DOCTYPE x [<!ENTITY xxe SYSTEM "file:///etc/passwd">]>'
            '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">&xxe;</Types>',
        )
    with pytest.raises(SampleError):
        parse("evil.xlsx", buffer.getvalue())


def test_a_json_array_is_one_class_named_by_its_type_and_geojson_is_a_geoproperty():
    sample = json.dumps(
        [
            {"id": "urn:ngsi-ld:Dock:hel.fi:d:1", "type": "BikeDock", "free": 3, "geometry": {"type": "Point", "coordinates": [24.9, 60.2]}, "tags": ["a"]},
            {"id": "urn:ngsi-ld:Dock:hel.fi:d:2", "type": "BikeDock", "free": 0, "geometry": {"type": "Point", "coordinates": [24.8, 60.1]}, "tags": []},
        ]
    ).encode()

    answer = infer("docks.json", sample)

    model = yaml.safe_load(answer["linkml"])
    assert list(model["classes"]) == ["BikeDock"]
    assert model["slots"]["geometry"]["annotations"] == {"ngsi_ld_kind": "GeoProperty"}
    assert model["slots"]["tags"]["annotations"] == {"ngsi_ld_kind": "ListProperty"}
    assert model["slots"]["free"] == {"range": "integer", "minimum_value": 0, "maximum_value": 3}
    assert answer["detectedTypes"]["id"] == "uriorcurie"


def test_a_json_object_of_arrays_is_one_class_per_key():
    sample = json.dumps({"stations": [{"name": "a", "docks": 4}], "readings": [{"value": 1.5}]}).encode()

    answer = infer("export.json", sample)

    assert list(yaml.safe_load(answer["linkml"])["classes"]) == ["Stations", "Readings"]


def pdf_of(lines: list[str]) -> bytes:
    """A one-page PDF with the lines as text, written by hand so the test owns its bytes."""
    stream = "BT /F1 12 Tf 20 280 Td 14 TL " + " ".join(f"({line}) Tj T*" for line in lines) + " ET"
    objects = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 300] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>",
        f"<< /Length {len(stream)} >>\nstream\n{stream}\nendstream",
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    ]
    out = "%PDF-1.4\n"
    offsets = []
    for number, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += f"{number} 0 obj\n{body}\nendobj\n"
    xref = len(out)
    out += f"xref\n0 {len(objects) + 1}\n0000000000 65535 f \n"
    out += "".join(f"{offset:010d} 00000 n \n" for offset in offsets)
    out += f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    return out.encode("latin-1")


def test_a_pdf_table_is_read_from_its_text():
    """DM-54: the first line with two or more cells is the header, the lines of the same width
    the rows; the reader resolves nothing outside the file."""
    sample = pdf_of(["Air quality report", "station  pm10  when", "north  12  2026-09-01", "south  15  2026-09-02", "Printed by the city"])

    answer = infer("report.pdf", sample)

    model = yaml.safe_load(answer["linkml"])
    assert model["classes"]["Report"]["slots"] == ["station", "pm10", "when"]
    assert model["slots"]["pm10"] == {"range": "integer", "minimum_value": 12, "maximum_value": 15}
    assert model["slots"]["when"]["range"] == "date"
    assert answer["rows"] == 2

    with pytest.raises(SampleError, match="no table"):
        parse("blank.pdf", pdf_of(["one line only"]))


def test_a_sample_past_the_cap_is_refused_before_it_is_parsed():
    with pytest.raises(SampleError, match="larger than"):
        parse("big.csv", b"a,b\n" + b"x" * infer_schema.MAX_SAMPLE_BYTES)


def test_the_route_answers_the_draft_and_refuses_what_it_cannot_read(no_network):
    """API/01 §11: `POST /infer-schema` takes the file's name and bytes, answers the draft, and
    reads the catalogue index it holds without fetching."""
    body = {"name": "sensors.csv", "content": base64.b64encode(SENSORS).decode()}

    status, answer = call("POST", "/infer-schema", body)

    assert status == 200
    assert "Sensors" in answer["linkml"]
    assert answer["operations"][0]["op"] == "addClass"
    assert answer["generatorVersion"].startswith("linkml-")

    status, answer = call("POST", "/infer-schema", {"name": "x.json", "content": base64.b64encode(b"nope").decode()})
    assert status == 400
    assert answer["errors"] == ["the file is not a JSON document"]

    status, answer = call("POST", "/infer-schema", {"name": "x.csv", "content": "***"})
    assert status == 400
    assert "base64" in answer["errors"][0]


def test_the_route_aligns_with_the_attributes_the_catalogue_already_holds(no_network):
    service.CATALOGUE._subjects = [
        {"name": "dataModel.Environment", "title": "Environment", "models": [{"id": "dataModel.Environment/AirQualityObserved", "name": "AirQualityObserved"}]}
    ]
    service.CATALOGUE._attributes = {"dataModel.Environment": {"AirQualityObserved": ["pm10", "temperature"]}}

    status, answer = call("POST", "/infer-schema", {"name": "sensors.csv", "content": base64.b64encode(SENSORS).decode()})

    assert status == 200
    assert set(answer["matches"]) == {"temperature", "pm10"}


def test_the_route_refuses_a_body_past_the_sample_cap_before_reading_it():
    environ: dict[str, object] = {
        "REQUEST_METHOD": "POST",
        "PATH_INFO": "/infer-schema",
        "CONTENT_LENGTH": str(service.MAX_INFER_BODY_BYTES + 1),
        "wsgi.input": _Explosive(),
    }
    from wsgiref.util import setup_testing_defaults

    setup_testing_defaults(environ)
    captured: dict[str, str] = {}
    chunks = service.application(environ, lambda status, headers: captured.update(status=status))
    assert captured["status"].startswith("413")
    assert b"larger than" in b"".join(chunks)


class _Explosive:
    def read(self, *args):  # pragma: no cover - the point is that it never runs
        raise AssertionError("the body was read")


def test_the_unit_crosswalk_anchors_every_code_it_offers():
    """DM-59: a UN/CEFACT code alone resolves to nothing, so every unit the inference can emit
    carries its QUDT unit and its quantity kind. The editor keeps the same table for the picker
    (`ui/src/pages/models/linkml.ts`, in the other repository); nothing can compare the two from
    inside one checkout, so each side guards its own shape and a code added here belongs there
    in the same change."""
    from infer_schema import UNIT_PREFIXES, UNIT_QUDT, UNIT_UCUM

    assert set(UNIT_QUDT) == set(UNIT_UCUM), set(UNIT_QUDT) ^ set(UNIT_UCUM)
    for code, (unit, kind) in UNIT_QUDT.items():
        assert unit and kind, code
        # Local names, never IRIs: the prefix is declared once in the model, and a full IRI
        # here would write it twice and let the two drift.
        assert ":" not in unit and "://" not in unit, code
        assert ":" not in kind and "://" not in kind, code
    # Every prefix the emitted CURIEs use, so `check_unit_prefixes` has something to find.
    assert {"ucefact", "qudt-unit", "qudt-quantkind"} <= set(UNIT_PREFIXES)
