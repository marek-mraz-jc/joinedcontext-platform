"""LinkML → one Markdown page per model (T-0415, DM-02, DM-43).

DM-02 commits `docs/{name}.md`: one file beside the source. LinkML's `gen-doc` writes a
directory — an index plus a page per class, slot and enum — which is a documentation site,
not the artifact this platform commits, and it renders neither the NGSI-LD kind of a slot nor
its UN/CEFACT unit, which are the two things a reader of one of these models most needs. So
the page is rendered here, from the same merged `SchemaView` every other generator reads.

Nothing in the output varies between two runs of the same source: a date or a timestamp here
would make every committed page stale the next day and CI red for it (DM-02).
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Any

from linkml_runtime import SchemaView
from linkml_runtime.linkml_model.meta import ClassDefinition, SlotDefinition

from common import ModelError, generator_version, load, ngsi_ld_kind, slots_of, unit_of


def _text(value: Any) -> str:
    """One cell: collapsed to a single line, with the table separator escaped."""
    if value is None:
        return ""
    return " ".join(str(value).split()).replace("|", "\\|")


def _term(view: SchemaView, element: Any) -> str:
    """The IRI of a class, slot or enum, as a CURIE where a prefix covers it."""
    expanded = view.get_uri(element, expand=True)
    return view.namespaces().curie_for(expanded, default_ok=False) or expanded


def _range(view: SchemaView, slot: SlotDefinition) -> str:
    """The range of a slot as a reader sees it: the type name, linked where it is local."""
    name = slot.range or view.schema.default_range or "string"
    local = name in view.all_classes() or name in view.all_enums()
    rendered = f"[`{name}`](#{name.lower()})" if local else f"`{name}`"
    return f"{rendered} (list)" if slot.multivalued else rendered


def _unit(slot: SlotDefinition) -> str:
    """The unit of a slot as symbol and code, which is what an export header carries (DM-06)."""
    unit = unit_of(slot)
    if not unit:
        return ""
    symbol = unit.get("symbol") or unit.get("ucumCode") or ""
    codes = ", ".join(unit.get("exactMappings") or [])
    return _text(f"{symbol} ({codes})" if symbol and codes else symbol or codes)


def _slot_rows(view: SchemaView, cls: ClassDefinition) -> list[str]:
    rows = []
    for slot in slots_of(view, cls):
        rows.append(
            "| `{name}` | {kind} | {range} | {required} | {unit} | `{iri}` | {description} |".format(
                name=slot.alias or slot.name,
                kind=ngsi_ld_kind(slot),
                range=_range(view, slot),
                required="yes" if slot.required else "",
                unit=_unit(slot),
                iri=_term(view, slot),
                description=_text(slot.description),
            )
        )
    return rows


def compile_docs(source: str | Path) -> str:
    """Render one LinkML document as one Markdown page."""
    view = load(source)
    schema = view.schema
    name = schema.name

    lines: list[str] = [
        f"# {_text(schema.title or name)}",
        "",
        "<!-- Generated from the LinkML source by Model Tools. Do not edit: `jcctl model",
        "     generate` overwrites this file and CI fails on any difference (DM-01, DM-02). -->",
        "",
    ]
    if schema.description:
        lines += [_text(schema.description), ""]

    facts = [f"- Namespace: `{schema.id}`", f"- Rendered by: `{generator_version()}`"]
    if schema.version:
        facts.append(f"- Version: `{schema.version}`")
    if schema.license:
        facts.append(f"- License: {schema.license}")
    lines += facts + [""]

    # Every class of the merged model, imports included: the JSON Schema and the SHACL shapes
    # are self-contained for the same reason, and a reader of `AirQualityObserved` needs to see
    # where `id` and `observedAt` come from.
    classes = list(view.all_classes().values())
    if not classes:
        raise ModelError("the model declares no class, so there is nothing to document")

    lines += ["## Classes", ""]
    for cls in classes:
        lines += [f"### {cls.name}", ""]
        if cls.description:
            lines += [_text(cls.description), ""]
        lines += [f"IRI: `{_term(view, cls)}`", ""]
        if cls.is_a:
            lines += [f"Specialises `{cls.is_a}`.", ""]
        rows = _slot_rows(view, cls)
        if rows:
            lines += [
                "| Attribute | NGSI-LD kind | Range | Required | Unit | IRI | Description |",
                "|---|---|---|---|---|---|---|",
                *rows,
                "",
            ]
        else:
            lines += ["This class declares no attributes.", ""]

    enums = list(view.all_enums().values())
    if enums:
        lines += ["## Enumerations", ""]
        for enum in enums:
            lines += [f"### {enum.name}", ""]
            if enum.description:
                lines += [_text(enum.description), ""]
            lines += ["| Value | Meaning |", "|---|---|"]
            for value in enum.permissible_values.values():
                lines.append(f"| `{value.text}` | {_text(value.description)} |")
            lines.append("")

    return "\n".join(lines).rstrip("\n") + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("source", help="LinkML YAML file")
    parser.add_argument("-o", "--output", help="write here instead of stdout")
    args = parser.parse_args(argv)

    try:
        page = compile_docs(args.source)
    except ModelError as err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    if args.output:
        Path(args.output).write_text(page, encoding="utf-8")
    else:
        sys.stdout.write(page)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
