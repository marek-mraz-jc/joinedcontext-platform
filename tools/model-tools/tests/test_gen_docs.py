"""The Markdown page DM-02 commits beside the source (T-0415, DM-02, DM-06, DM-43)."""

from __future__ import annotations

import re

import pytest

from common import ModelError
from gen_docs import compile_docs


def _table(page: str, heading: str) -> list[str]:
    """The rows of the table under one H3, without its header and separator."""
    section = page.split(f"### {heading}\n", 1)[1].split("\n### ", 1)[0]
    return [line for line in section.splitlines() if line.startswith("| `")]


def test_the_page_is_one_document_with_a_title_and_the_namespace(senzor):
    page = compile_docs(senzor)
    assert page.startswith("# Ovzdusie sensor readings\n")
    assert "- Namespace: `https://banskabystrica.sk/ns/senzor`" in page
    assert page.endswith("\n") and not page.endswith("\n\n")


def test_every_attribute_carries_its_ngsi_ld_kind_and_its_unit(senzor):
    rows = _table(compile_docs(senzor), "AirQualityObserved")
    temperature = next(row for row in rows if row.startswith("| `temperature`"))

    # The two columns `gen-doc` does not render, and the reason this generator exists.
    assert "| Property |" in temperature
    # Both mappings: the UN/CEFACT code a reader puts on the wire, and the QUDT unit a reader
    # dereferences to align it with somebody else's (DM-06, DM-59).
    assert "°C (unece:CEL, qudt-unit:DEG_C)" in temperature
    assert "| yes |" in temperature

    kinds = {row.split("|")[1].strip().strip("`"): row.split("|")[2].strip() for row in rows}
    assert kinds["refDevice"] == "Relationship"
    assert kinds["label"] == "LanguageProperty"
    assert kinds["address"] == "JsonProperty"
    assert kinds["location"] == "GeoProperty", "inherited from ngsi-ld-core"


def test_the_imported_core_attributes_are_documented_too(senzor):
    """The artifacts describe the merged model, so `id` and `observedAt` are on the page."""
    rows = _table(compile_docs(senzor), "AirQualityObserved")
    assert any(row.startswith("| `observedAt`") for row in rows)
    assert "### Entity" in compile_docs(senzor)


def test_an_enumeration_lists_its_permissible_values(senzor):
    page = compile_docs(senzor)
    section = page.split("### ReliabilityLevel\n", 1)[1]
    assert "| `low` |" in section and "| `high` |" in section


def test_the_page_says_it_is_generated_so_nobody_edits_it(senzor):
    page = compile_docs(senzor)
    assert "Do not edit" in page and "DM-02" in page


def test_the_page_carries_nothing_that_moves_with_the_clock(senzor):
    """A committed artifact is compared byte for byte, so a date in it is a daily red lane."""
    page = compile_docs(senzor)
    assert compile_docs(senzor) == page
    assert not re.search(r"\b20\d\d-\d\d-\d\d\b", page), "no rendering date on the page"


def test_a_pipe_in_a_description_does_not_break_the_table(tmp_path):
    source = tmp_path / "pipe.linkml.yaml"
    source.write_text(
        "id: https://example.org/ns/pipe\n"
        "name: pipe\n"
        "prefixes: {linkml: 'https://w3id.org/linkml/', ex: 'https://example.org/ns/'}\n"
        "default_prefix: ex\n"
        "default_range: string\n"
        "imports: [linkml:types]\n"
        "classes:\n"
        "  Thing:\n"
        "    class_uri: ex:Thing\n"
        "    attributes:\n"
        "      label:\n"
        "        slot_uri: ex:label\n"
        "        description: |-\n"
        "          one | two\n"
        "          and a second line\n",
        encoding="utf-8",
    )
    row = _table(compile_docs(str(source)), "Thing")[0]
    # Seven columns is eight unescaped separators; the one in the description is escaped
    # and the newline is gone, so the row still parses as one row of the same table.
    assert row.replace(r"\|", "").count("|") == 8, row
    assert "one \\| two and a second line" in row


def test_a_model_without_a_class_is_refused_rather_than_rendered_empty(tmp_path):
    source = tmp_path / "empty.linkml.yaml"
    source.write_text(
        "id: https://example.org/ns/empty\nname: empty\n"
        "prefixes: {linkml: 'https://w3id.org/linkml/'}\ndefault_range: string\n"
        "imports: [linkml:types]\n",
        encoding="utf-8",
    )
    with pytest.raises(ModelError, match="no class"):
        compile_docs(str(source))
