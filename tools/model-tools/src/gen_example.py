"""LinkML → one validated example entity (T-0415, DM-02, DM-21, DM-43).

DM-02 commits `examples/{name}.example.jsonld` beside the source and DM-21 says what makes it
worth committing: it validates against the generated JSON Schema and every member it carries
is defined by the generated `@context`. Both are asserted here, at generation time, so an
example that would mislead a reader is a compile error and not a file.

The example is the **key-value** form of one entity, which is the form the JSON Schema
describes and the form the Portal's preview panel feeds to the generated form. Values are
derived from the ranges and are fixed: a date that moved with the clock would make every
committed example stale the next day and CI red for it.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

from linkml_runtime import SchemaView
from linkml_runtime.linkml_model.meta import ClassDefinition, SlotDefinition

from common import ModelError, load, ngsi_ld_kind, slots_of
from gen_context import compile_context
from gen_json_schema import compile_schema

#: Fixed stand-in values, one per LinkML base type. Fixed, not generated: the artifact is
#: committed and compared byte for byte (DM-02), so anything that moves is a daily red lane.
SCALARS: dict[str, Any] = {
    "integer": 0,
    "float": 0.0,
    "double": 0.0,
    "decimal": 0.0,
    "boolean": True,
    "date": "2026-01-01",
    "datetime": "2026-01-01T00:00:00Z",
    "date_or_datetime": "2026-01-01T00:00:00Z",
    "time": "00:00:00",
}

#: A GeoProperty carries a GeoJSON geometry object, which is what the JSON Schema describes for
#: the kind rather than for the range (T-0416).
GEOMETRY = {"type": "Point", "coordinates": [0.0, 0.0]}

#: A LanguageProperty carries a language map. The model declares no locale, so the example uses
#: the one every instance of the platform offers.
EXAMPLE_LANGUAGE = "en"

#: The space segment of the example entity id. The LinkML source knows the organisation (it is
#: the host of the schema id) and cannot know the Context Space the model will be used in, so
#: the id is syntactically what PF-42 requires with a segment that says it is an example.
EXAMPLE_SPACE = "example"
EXAMPLE_LOCAL_ID = "1"

#: How deep an inline object may nest before the model is refusing to terminate.
MAX_DEPTH = 8

#: JSON-LD keywords an example carries that a term definition never defines (DM-21).
KEYWORDS = ("@context", "id", "type")


def _domain(schema_id: str) -> str:
    """The organisation domain of a model: the host of its namespace IRI."""
    host = urlparse(schema_id).netloc
    return host or "example.invalid"


def entity_class(view: SchemaView) -> ClassDefinition:
    """The class the example is an instance of: the first entity class the model declares.

    A model usually declares one NGSI-LD entity type; where it declares several, the example is
    of the first, because DM-02 commits one example file and the Portal preview renders one
    entity. The Markdown page documents all of them.
    """
    for cls in view.all_classes().values():
        if cls.name != "Entity" and "Entity" in view.class_ancestors(cls.name):
            return cls
    raise ModelError(
        "no class of this model specialises `Entity`, so there is no entity to make an example "
        "of; a model whose classes are all inline objects declares no NGSI-LD type (DM-09)"
    )


def _scalar(view: SchemaView, slot: SlotDefinition, name: str) -> Any:
    """One value for a slot whose range is a type or an enum."""
    enum = view.all_enums().get(name)
    if enum is not None:
        values = list(enum.permissible_values)
        if not values:
            raise ModelError(f"enum `{name}` permits no value, so `{slot.name}` has none")
        return values[0]

    base = name
    seen = set()
    # Follow `typeof` down to the base type: a model may declare `Celsius` as a float.
    while base in view.all_types() and base not in seen:
        seen.add(base)
        declared = view.all_types()[base]
        if declared.base in SCALARS or declared.base == "str":
            base = declared.base
            break
        base = declared.typeof or base
    if base in SCALARS:
        value = SCALARS[base]
        # A bound the slot declares is what the JSON Schema checks the example against (DM-21):
        # the lowest permitted value is always inside it.
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            if slot.minimum_value is not None:
                return type(value)(slot.minimum_value)
            if slot.maximum_value is not None:
                return type(value)(slot.maximum_value)
        return value
    if slot.pattern:
        return _from_pattern(slot.pattern) or (slot.alias or slot.name)
    return slot.alias or slot.name


#: The pattern atoms an example can be spelled from: one character class, with or without a
#: count, or a literal. A pattern written any other way keeps the slot name as its example.
PATTERN_ATOM = re.compile(r"\[([A-Za-z0-9])-([A-Za-z0-9])\](?:\{(\d+)\})?|\\(.)|([^\\\[\]{}()|*+?^$])")


def _from_pattern(pattern: str) -> str | None:
    """A string matching a simple pattern, `^[A-Z]{2}[0-9]{3}$` and the like; None otherwise."""
    body = pattern.removeprefix("^").removesuffix("$")
    out, position = "", 0
    for atom in PATTERN_ATOM.finditer(body):
        if atom.start() != position:
            return None
        low, high, count, escaped, literal = atom.groups()
        if low is not None:
            out += low * int(count or 1)
        else:
            out += escaped if escaped is not None else literal
        position = atom.end()
    if position != len(body) or not re.fullmatch(pattern, out):
        return None
    return out


def _value(view: SchemaView, slot: SlotDefinition, domain: str, depth: int) -> Any:
    """The example value of one slot, in the key-value form the JSON Schema describes."""
    if slot.examples:
        # The model author wrote one. Theirs beats anything derived from the range.
        return slot.examples[0].value

    kind = ngsi_ld_kind(slot)
    range_name = slot.range or view.schema.default_range or "string"
    target = view.all_classes().get(range_name)

    if kind == "Relationship":
        # The object of a Relationship is the id of another entity, never an inline object.
        entity_type = target.name if target is not None else "Entity"
        value: Any = _urn(entity_type, domain)
    elif kind == "GeoProperty":
        value = dict(GEOMETRY)
    elif kind == "LanguageProperty":
        # A language map, which is the shape the JSON Schema describes for the kind (T-0416).
        value = {EXAMPLE_LANGUAGE: _scalar(view, slot, range_name)}
    elif target is not None:
        value = _object(view, target, domain, depth + 1)
    else:
        value = _scalar(view, slot, range_name)

    return [value] if slot.multivalued else value


def _urn(entity_type: str, domain: str) -> str:
    """One entity id in the platform's scheme (PF-42)."""
    return f"urn:ngsi-ld:{entity_type}:{domain}:{EXAMPLE_SPACE}:{EXAMPLE_LOCAL_ID}"


def _object(view: SchemaView, cls: ClassDefinition, domain: str, depth: int) -> dict[str, Any]:
    """An inline object of one class: every slot it declares, in declaration order."""
    if depth > MAX_DEPTH:
        raise ModelError(
            f"`{cls.name}` nests inline objects more than {MAX_DEPTH} deep, which a model that "
            "terminates does not do"
        )
    body: dict[str, Any] = {}
    for slot in slots_of(view, cls):
        name = slot.alias or slot.name
        if name in ("id", "type"):
            continue
        body[name] = _value(view, slot, domain, depth)
    return body


def compile_example(source: str | Path) -> dict[str, Any]:
    """Render one LinkML document as one validated example entity (DM-21)."""
    view = load(source)
    cls = entity_class(view)
    domain = _domain(view.schema.id or "")

    example: dict[str, Any] = {
        "id": _urn(cls.name, domain),
        "type": cls.name,
    }
    example.update(_object(view, cls, domain, 0))

    _check_against_schema(compile_schema(source), cls.name, example)
    _check_against_context(compile_context(source), example)
    return example


def _check_against_schema(schema: dict[str, Any], class_name: str, example: dict[str, Any]) -> None:
    """DM-21: the example validates against the JSON Schema generated from the same source."""
    import jsonschema

    definitions = schema.get("definitions") or {}
    if class_name not in definitions:
        raise ModelError(f"the JSON Schema defines no `{class_name}` to validate the example")
    # The whole document is the resolution scope, so `$ref`s between definitions resolve.
    against = dict(schema)
    against.pop("additionalProperties", None)
    against["$ref"] = f"#/definitions/{class_name}"
    errors = sorted(
        jsonschema.Draft7Validator(against).iter_errors(example), key=lambda e: list(e.path)
    )
    if errors:
        raise ModelError(
            "the generated example does not validate against the generated JSON Schema (DM-21): "
            + "; ".join(f"{'/'.join(str(p) for p in e.path) or '(root)'}: {e.message}" for e in errors)
        )


def _check_against_context(context: dict[str, Any], example: dict[str, Any]) -> None:
    """DM-21: every member of the example is a term the generated `@context` defines."""
    terms = context.get("@context") or {}
    unmapped = [key for key in example if key not in KEYWORDS and key not in terms]
    if unmapped:
        raise ModelError(
            "the generated example carries members the generated @context does not define, so it "
            f"would expand as opaque data (DM-21): {', '.join(sorted(unmapped))}"
        )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("source", help="LinkML YAML file")
    parser.add_argument("-o", "--output", help="write here instead of stdout")
    args = parser.parse_args(argv)

    try:
        example = compile_example(args.source)
    except ModelError as err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    text = json.dumps(example, indent=2, sort_keys=True) + "\n"
    if args.output:
        Path(args.output).write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
