"""LinkML → JSON Schema draft-07 (T-0168, T-0416, DM-02, DM-03, DM-05, DM-18, TS-18).

`gen-json-schema` renders 2019-09. The gateway validates every write against these schemas
with a draft-07 validator (CC-12, stack verdict S4), and a 2019-09 keyword there is not a
validation error but a silently ignored constraint, so the dialect is converted here rather
than hoped for.

It also knows nothing about NGSI-LD, and for two kinds the JSON shape is not the LinkML range:
a LanguageProperty carries a language map and a GeoProperty a GeoJSON geometry, both objects
where the range is a string. The `@context` and the SHACL shapes are already driven by
`ngsi_ld_kind` for the same reason (DM-05); this generator follows the same rule, because a
schema that calls a language map a string rejects exactly what ETSI 9.3.2.3 prescribes.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

from linkml.generators.jsonschemagen import JsonSchemaGenerator
from linkml_runtime import SchemaView

import yaml

from common import ModelError, as_path, generator_version, load, ngsi_ld_kind, slots_of, unit_of

DRAFT_07 = "http://json-schema.org/schema#"
DRAFT_07_ID = "http://json-schema.org/draft-07/schema#"

#: 2019-09 and 2020-12 keywords that have no draft-07 equivalent. Dropping them is the honest
#: move: keeping a keyword the validator ignores would claim a constraint nothing enforces.
UNSUPPORTED = (
    "unevaluatedProperties",
    "unevaluatedItems",
    "$anchor",
    "$recursiveRef",
    "$recursiveAnchor",
    "$dynamicRef",
    "$dynamicAnchor",
    "$vocabulary",
    "prefixItems",
    "minContains",
    "maxContains",
    "deprecated",
)

#: Keywords 2019-09 split out of draft-07's `dependencies`, which draft-07 spells as one.
DEPENDENCY_KEYWORDS = ("dependentRequired", "dependentSchemas")


def _to_draft_07(node: Any) -> Any:
    """Rewrite one JSON Schema node from 2019-09 into draft-07."""
    if isinstance(node, list):
        return [_to_draft_07(item) for item in node]
    if not isinstance(node, dict):
        return node

    out: dict[str, Any] = {}
    for key, value in node.items():
        if key in UNSUPPORTED:
            continue
        if key == "$defs":
            out["definitions"] = _to_draft_07(value)
        elif key in DEPENDENCY_KEYWORDS:
            # Both collapse into draft-07 `dependencies`, which takes a list or a schema per
            # property; merging keeps whichever the source produced.
            out.setdefault("dependencies", {}).update(_to_draft_07(value))
        elif key == "$schema":
            out[key] = DRAFT_07_ID
        elif key == "$ref" and isinstance(value, str):
            out[key] = value.replace("#/$defs/", "#/definitions/")
        else:
            out[key] = _to_draft_07(value)
    return out


#: The GeoJSON geometry types an NGSI-LD GeoProperty value may take. `GeometryCollection` is
#: left out: it carries `geometries` instead of `coordinates`, and no geo-query operator of
#: ETSI 4.10 is defined over one, so accepting it would promise a query that cannot run.
GEOMETRY_TYPES = (
    "Point",
    "MultiPoint",
    "LineString",
    "MultiLineString",
    "Polygon",
    "MultiPolygon",
)


def _value_node(prop: dict[str, Any]) -> dict[str, Any]:
    """The node describing one value: an array's `items` where the slot is multivalued."""
    declared = prop.get("type")
    array = declared == "array" or (isinstance(declared, list) and "array" in declared)
    items = prop.get("items")
    return items if array and isinstance(items, dict) else prop


def _shape_of(kind: str, node: dict[str, Any]) -> dict[str, Any] | None:
    """The JSON shape of one NGSI-LD kind, or None where the range already describes it.

    A Property and a Relationship are what LinkML rendered: a value of the declared type, and
    an entity id, which is a string either way. The other two are objects.
    """
    declared = node.get("type")
    nullable = isinstance(declared, list) and "null" in declared
    types: Any = ["object", "null"] if nullable else "object"

    if kind == "LanguageProperty":
        # A language map: one string per language tag, keys the model cannot enumerate.
        return {"type": types, "additionalProperties": {"type": "string"}}
    if kind == "GeoProperty":
        return {
            "type": types,
            "properties": {
                "type": {"type": "string", "enum": list(GEOMETRY_TYPES)},
                "coordinates": {"type": "array"},
            },
            "required": ["type", "coordinates"],
        }
    return None


def _annotate(schema: dict[str, Any], view: SchemaView) -> dict[str, Any]:
    """Carry the NGSI-LD kind and the UN/CEFACT unit into the schema (DM-05, DM-06).

    Both are `x-` keywords: a draft-07 validator ignores unknown keywords, so the metadata
    travels with the schema without changing what it validates. Exports read the unit for the
    column header, the editor reads the kind to pick a form control (DM-20).

    The kind also decides the shape of a LanguageProperty and of a GeoProperty, which the
    range cannot express, so those two are rewritten here rather than described wrongly.
    """
    definitions = schema.get("definitions", {})
    for cls in view.all_classes().values():
        target = definitions.get(cls.name)
        if not isinstance(target, dict):
            continue
        properties = target.get("properties", {})
        for slot in slots_of(view, cls):
            prop = properties.get(slot.alias or slot.name)
            if not isinstance(prop, dict):
                continue
            kind = ngsi_ld_kind(slot)
            node = _value_node(prop)
            shape = _shape_of(kind, node)
            if shape is not None:
                # The description is the only thing worth keeping from a node that described
                # the wrong type; every constraint on it was about a string.
                description = node.get("description")
                node.clear()
                node.update(shape)
                if description:
                    node["description"] = description

            prop["x-ngsi-ld-kind"] = kind
            unit = unit_of(slot)
            if unit:
                prop["x-unit"] = unit
    return schema


def _text(value: Any) -> str | dict[str, str] | None:
    """A title or description as a person reads it: a string, or a map of language to string."""
    if isinstance(value, str):
        return value.strip() or None
    if isinstance(value, dict):
        texts = {str(tag): text for tag, text in value.items() if isinstance(text, str) and text.strip()}
        return texts or None
    return None


def _enum_titles(schema: dict[str, Any], raw: Any) -> dict[str, Any]:
    """Carry each permissible value's title and description into its enum (UI-86).

    `gen-json-schema` writes an enum as its bare values, so the grid and the forms could show a
    person only `traffic`. The titles travel as `x-enum-titles` and `x-enum-descriptions`,
    keyed by value, which a draft-07 validator ignores. They are read from the YAML itself: a
    title per language is a map there, which LinkML's own model flattens into its repr.
    """
    definitions = schema.get("definitions", {})
    enums = raw.get("enums") if isinstance(raw, dict) else None
    for name, enum in (enums or {}).items():
        target = definitions.get(name)
        values = enum.get("permissible_values") if isinstance(enum, dict) else None
        if not isinstance(target, dict) or not isinstance(values, dict):
            continue
        for key, member in (("x-enum-titles", "title"), ("x-enum-descriptions", "description")):
            found = {
                str(value): text
                for value, details in values.items()
                if isinstance(details, dict) and (text := _text(details.get(member))) is not None
            }
            if found:
                target[key] = found
    return schema


def compile_schema(source: str | Path) -> dict[str, Any]:
    """Render one LinkML document as a draft-07 JSON Schema."""
    view = load(source)
    with as_path(source) as path:
        raw = yaml.safe_load(Path(path).read_text(encoding="utf-8"))
    # `not_closed=False` is DM-28's default: a payload with an undeclared attribute is refused
    # unless the class opts into an open world.
    rendered = JsonSchemaGenerator(view.schema, not_closed=False).serialize()
    schema = _to_draft_07(json.loads(rendered))
    schema["$schema"] = DRAFT_07_ID
    schema["x-generator-version"] = generator_version()
    return _enum_titles(_annotate(schema, view), raw)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("source", help="LinkML YAML file")
    parser.add_argument("-o", "--output", help="write here instead of stdout")
    args = parser.parse_args(argv)

    try:
        schema = compile_schema(args.source)
    except ModelError as err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    text = json.dumps(schema, indent=2, sort_keys=True) + "\n"
    if args.output:
        Path(args.output).write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
