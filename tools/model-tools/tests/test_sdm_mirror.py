"""The mirror of the catalogue (T-6353): three models from the recorded fixtures, no network."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Self

import pytest
import yaml

import sdm_mirror

FIXTURES = Path(__file__).parent / "fixtures"
GOLDEN = FIXTURES / "sdm-mirror"
SUBJECT = {"name": "dataModel.Sample", "title": "Sample"}
MODELS = ["dataModel.Sample/Good", "dataModel.Sample/Nested", "dataModel.Sample/Broken"]
NOW = "2026-11-02T00:00:00Z"


def fetched(model: str) -> dict:
    """What `fetch` answers, from documents written here: Broken has no schema to read."""
    name = model.split("/")[1]
    if name == "Broken":
        raise ValueError("https://raw.githubusercontent.com/... answered 404")
    properties = {"pm10": {"type": "number", "description": "Property. PM10"}}
    if name == "Nested":
        phases = {"L1": {"type": "number", "description": "Property. Line 1"}}
        properties["voltage"] = {"type": "object", "description": "Property. Voltage", "properties": phases}
    return {
        "schema": {
            "description": f"{name} sample",
            "allOf": [{"properties": properties}],
            "required": ["id", "type"],
        },
        "context": {"@context": {"pm10": "https://smartdatamodels.org/dataModel.Sample/pm10"}},
        "example": None,
        "commons": None,
        "provenance": {
            "repository": "https://github.com/smart-data-models/dataModel.Sample",
            "path": f"{name}/schema.json",
            "commit": "a" * 40,
        },
    }


@pytest.fixture
def mirror(tmp_path) -> Path:
    results = [sdm_mirror.convert_one(model, fetched) for model in MODELS]
    subject = {**SUBJECT, "models": [{"id": m, "name": m.split("/")[1]} for m in MODELS]}
    sdm_mirror.write(tmp_path, [subject], results, NOW)
    return tmp_path


def test_a_converted_model_matches_its_golden_linkml(mirror):
    for name in ("Good", "Nested"):
        written = (mirror / "dataModel.Sample" / name / "linkml.yaml").read_text(encoding="utf-8")
        assert written == (GOLDEN / f"{name}.linkml.yaml").read_text(encoding="utf-8")


def test_the_index_pins_each_model_and_lists_the_failed_with_their_finding(mirror):
    index = json.loads((mirror / "index.json").read_text(encoding="utf-8"))
    assert [m["name"] for m in index["subjects"][0]["models"]] == ["Good", "Nested"]
    assert index["subjects"][0]["models"][0]["commit"] == "a" * 40
    assert index["subjects"][0]["models"][1]["attributes"] == ["pm10", "voltage"]
    assert index["failed"] == [
        {"id": "dataModel.Sample/Broken", "finding": "https://raw.githubusercontent.com/... answered 404"}
    ]


def test_a_failed_model_has_no_directory_to_offer(mirror):
    assert not (mirror / "dataModel.Sample" / "Broken").exists()


def test_a_value_structure_keeps_its_upstream_number_type(mirror):
    document = yaml.safe_load((mirror / "dataModel.Sample" / "Nested" / "linkml.yaml").read_text(encoding="utf-8"))
    nested = next(c for c in document["classes"].values() if "L1" in c.get("attributes", {}))
    assert nested["attributes"]["L1"]["range"] == "float"


def test_a_night_without_change_keeps_the_time_so_nothing_is_committed(mirror):
    results = [sdm_mirror.convert_one(model, fetched) for model in MODELS]
    subject = {**SUBJECT, "models": [{"id": m, "name": m.split("/")[1]} for m in MODELS]}
    sdm_mirror.write(mirror, [subject], results, "2026-11-03T00:00:00Z")
    assert json.loads((mirror / "index.json").read_text(encoding="utf-8"))["refreshedAt"] == NOW
    results[0]["commit"] = "b" * 40
    sdm_mirror.write(mirror, [subject], results, "2026-11-03T00:00:00Z")
    assert json.loads((mirror / "index.json").read_text(encoding="utf-8"))["refreshedAt"] == "2026-11-03T00:00:00Z"


def test_a_run_past_the_failure_line_leaves_yesterdays_mirror(tmp_path, monkeypatch):
    (tmp_path / "index.json").write_text("{}", encoding="utf-8")
    subject = {**SUBJECT, "models": [{"id": "dataModel.Sample/Broken", "name": "Broken"}]}
    monkeypatch.setattr(sdm_mirror, "fetch_catalogue", lambda: [subject])
    monkeypatch.setattr(sdm_mirror, "ProcessPoolExecutor", _Inline)
    assert sdm_mirror.main(["--out", str(tmp_path)]) == 2
    assert (tmp_path / "index.json").read_text(encoding="utf-8") == "{}"


class _Inline:
    """The pool in this process: a monkeypatched module does not cross a fork on every platform."""

    def __init__(self, _jobs: int) -> None:
        pass

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_: object) -> None:
        pass

    def map(self, function, items, chunksize: int = 1):
        return [sdm_mirror.convert_one(item, fetched) for item in items]
