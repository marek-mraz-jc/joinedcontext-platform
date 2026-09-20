"""LinkML → TypeScript row types for generated applications (T-0677, SDK-03, SDK-10, DM-19).

A generated application imports `src/jc-types.ts` with `import type` and hands its types to the
App SDK: `useEntities<AirQualityObserved>("AirQualityObserved")`. The SDK does not return NGSI-LD
entities but rows, every attribute reduced to one cell (SDK-03): a GeoProperty is a GeoJSON
geometry, a DateTime, a LanguageProperty and a Relationship are strings, a number is a number,
and a list or an inline object is the text the table shows. The types describe that row, so the
compiler checks what the application actually reads.

LinkML's `gen-typescript` renders the model's own ranges as interfaces with runtime enums. Three
things in that output do not fit a row: an interface has no implicit index signature and is not
assignable to the SDK's `Row`, a TypeScript `enum` is runtime code that `import type` erases,
and an inline class or a language map is not the cell the SDK hands over. So this generator
keeps LinkML's `TypescriptGenerator` for loading and naming and replaces its range mapping and
template with the row shape, driven by `ngsi_ld_kind` like every other artifact (DM-05).
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

from linkml.generators.typescriptgen import TypescriptGenerator
from linkml_runtime import SchemaView
from linkml_runtime.linkml_model.meta import ClassDefinition, SlotDefinition

from common import (
    ModelError,
    annotation_value,
    generator_version,
    load,
    ngsi_ld_kind,
    slots_of,
    unit_of,
)

#: The TypeScript type of a GeoProperty cell. Structurally the SDK's `Geo`, declared here so
#: the file needs no import and stays valid for a function that only imports types.
GEOMETRY = "Geometry"

#: A cell of an attribute the model does not declare, for an open-world class (DM-28).
ANY_CELL = f"string | number | boolean | {GEOMETRY} | null"

#: LinkML types whose values are JSON numbers and booleans. Everything else, dates and times
#: included, reaches a row as a string.
NUMBER_TYPES = {"integer", "float", "double", "decimal"}
BOOLEAN_TYPES = {"boolean"}

#: Names this file declares itself, which a class or an enum of the model must not take.
RESERVED_NAMES = {GEOMETRY, "EntityTypeName"}

IDENTIFIER = re.compile(r"^[A-Za-z_$][A-Za-z0-9_$]*$")


class RowTypesGenerator(TypescriptGenerator):
    """`TypescriptGenerator` with the App SDK's row shape instead of the LinkML range."""

    def range(self, slot: SlotDefinition) -> str:
        """The TypeScript type of one attribute's cell."""
        view = self.schemaview
        kind = ngsi_ld_kind(slot)
        if kind == "GeoProperty":
            return GEOMETRY
        # The SDK joins a list into one text, and a Relationship, a language map, a JSON
        # object or a vocabulary term is read as text too.
        if slot.multivalued or kind != "Property":
            return "string"
        name = slot.range
        if name in view.all_enums():
            return self.name(view.get_enum(name))
        if name in view.all_types():
            ancestors = set(view.type_ancestors(name))
            if ancestors & NUMBER_TYPES:
                return "number"
            if ancestors & BOOLEAN_TYPES:
                return "boolean"
        # A string, a date, a URI, or an inline class the SDK shows as its first text.
        return "string"

    def serialize(self, output: str | None = None) -> str:
        view = self.schemaview
        enums = sorted(view.all_enums().values(), key=lambda e: e.name)
        entities = [cls for cls in view.all_classes().values() if _is_entity(view, cls)]

        taken = [self.name(e) for e in enums] + [self.name(c) for c in entities]
        for name in taken:
            if name in RESERVED_NAMES:
                raise ModelError(f"'{name}' is a name jc-types.ts declares itself; rename the class or enum")

        lines = [
            f"// Rendered by Model Tools ({generator_version()}) from the endpoint's LinkML model. Do not edit.",
            "// Row shapes of @joinedcontext/sdk (SDK-03): a GeoProperty is a GeoJSON geometry; a DateTime,",
            "// a LanguageProperty, a Relationship, a list and an inline object are strings.",
            "",
            f"export type {GEOMETRY} = {{ type: string; coordinates: unknown }};",
        ]
        for enum in enums:
            values = " | ".join(json.dumps(str(v)) for v in enum.permissible_values) or "never"
            lines += ["", *_doc(enum.description), f"export type {self.name(enum)} = {values};"]
        for cls in entities:
            lines += ["", *self._entity(cls)]
        names = " | ".join(json.dumps(cls.name) for cls in entities) or "never"
        lines += ["", f"export type EntityTypeName = {names};", ""]

        text = "\n".join(lines)
        if output is not None:
            Path(output).write_text(text, encoding="utf-8")
        return text

    def _entity(self, cls: ClassDefinition) -> list[str]:
        view = self.schemaview
        body = ["  id: string;", f"  type: {json.dumps(cls.name)};"]
        for slot in slots_of(view, cls):
            key = slot.alias or slot.name
            if key in ("id", "type"):
                continue
            unit = unit_of(slot)
            symbol = unit and (unit.get("symbol") or unit.get("ucumCode"))
            text = " ".join(filter(None, [slot.description, f"Unit: {symbol}." if symbol else None]))
            body += [f"  {line}" for line in _doc(text)]
            prop = key if IDENTIFIER.match(key) else json.dumps(key)
            cell = self.range(slot)
            body.append(f"  {prop}: {cell};" if slot.required else f"  {prop}?: {cell} | null;")

        declared = [*_doc(cls.description), f"export type {self.name(cls)} = {{", *body, "}"]
        if (annotation_value(cls, "open_world") or "").lower() == "true":
            # An intersection, not an index signature in the literal: optional members would
            # have to include `undefined` in it, and then the type is no longer a `Row`.
            declared[-1] = f"}} & {{ [attr: string]: {ANY_CELL} }}"
        declared[-1] += ";"
        return declared


def _is_entity(view: SchemaView, cls: ClassDefinition) -> bool:
    """A class an endpoint serves entities of: it specialises `Entity` and can have instances."""
    return cls.name != "Entity" and not cls.abstract and not cls.mixin and "Entity" in view.class_ancestors(cls.name)


def _doc(text: str | None) -> list[str]:
    if not text:
        return []
    flat = " ".join(text.split()).replace("*/", "*\\/")
    return [f"/** {flat} */"]


def compile_typescript(source: str | Path) -> str:
    """Render one LinkML document as the `jc-types.ts` of a generated application."""
    view = load(source)
    return RowTypesGenerator(view.schema).serialize()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("source", help="LinkML YAML file")
    parser.add_argument("-o", "--output", help="write here instead of stdout")
    args = parser.parse_args(argv)

    try:
        text = compile_typescript(args.source)
    except ModelError as err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    if args.output:
        Path(args.output).write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
