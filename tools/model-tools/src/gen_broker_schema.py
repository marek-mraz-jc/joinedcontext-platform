"""LinkML → the schema a broker Tenant's views read (T-3703, T-3628, DM-19).

The Antares broker takes one LinkML schema per Tenant at `PUT /q/schemas/{tenant}`, "in its
JSON serialisation, resolved (imports and `is_a` folded)", and reads it with its own
`linkml-model` crate: a subset of LinkML whose every struct denies unknown fields, so a
metaslot outside it fails the surface instead of being dropped. LinkML's own materialised JSON
does not fit: it carries `@type`, `types`, `attributes`, `default_range` and the temporary
`source_file` of the request. This renders the merged model onto that subset:

- every class keeps `class_uri`, `is_a` and `description`, and lists all its slots, inherited,
  mixed in and attribute ones included; the broker walks `is_a` and skips a name twice;
- every slot, attribute ones included, is a top-level slot, and what a class changes about it
  (`slot_usage`, a mixin, an attribute that differs from a same-named one) is that class's
  `slot_usage`, holding only the metaslots that differ;
- a range is a class, an enum or one of the broker's built-in types: a LinkML type is walked
  down `typeof` to the built-in it specialises, and one that never reaches one is refused;
- an annotation value is text, `true`/`false` for a boolean, as the broker's flags read it;
- a unit keeps `ucum_code`, `symbol` and `exact_mappings`;
- a slot's `rank` and `title`, the display order and name a table reads (DM-98, T-4672), stay.

The Portal's reconciler sends the result to the broker of a Context Space whose `dataModelRef`
is set; Model Tools renders it with the other artifacts so the editor and the broker cannot
disagree about what a model is.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

import jsonasobj2
from linkml_runtime import SchemaView
from linkml_runtime.linkml_model.meta import SlotDefinition

from common import ModelError, load

#: The built-in types of the broker's subset (Antares `linkml-model`, `Builtin`).
BUILTINS = (
    "string",
    "integer",
    "float",
    "double",
    "decimal",
    "boolean",
    "date",
    "datetime",
    "time",
    "uri",
    "uriorcurie",
)

#: The slot metaslots of the subset that are plain values, in the order they are written.
SLOT_VALUES = (
    "slot_uri",
    "range",
    "required",
    "multivalued",
    "identifier",
    "inlined",
    "inverse",
    "description",
    "rank",
    "title",
)


def _text(value: Any) -> str:
    """An annotation value as the broker reads it: text, a boolean as `true` or `false`."""
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, str):
        return value
    return json.dumps(jsonasobj2.as_dict(value) if isinstance(value, jsonasobj2.JsonObj) else value)


def _annotations(element: Any) -> dict[str, dict[str, str]]:
    raw = getattr(element, "annotations", None)
    entries = jsonasobj2.as_dict(raw) if raw else {}
    out: dict[str, dict[str, str]] = {}
    for tag, entry in entries.items():
        value = entry.get("value") if isinstance(entry, dict) else getattr(entry, "value", entry)
        out[str(tag)] = {"tag": str(tag), "value": _text(value)}
    return out


def _range(view: SchemaView, name: str, owner: str) -> str:
    """A class, an enum or the built-in a LinkML type specialises."""
    if name in view.all_classes() or name in view.all_enums():
        return name
    seen: set[str] = set()
    current: str | None = name
    while current is not None and current not in BUILTINS:
        declared = view.get_type(current)
        if declared is None or current in seen:
            break
        seen.add(current)
        current = declared.typeof
    if current not in BUILTINS:
        raise ModelError(
            f"{owner}: range '{name}' is not a class, an enum or a type based on one of "
            f"{', '.join(BUILTINS)}, which is what a broker Tenant's schema can hold"
        )
    return current


def _slot(view: SchemaView, slot: SlotDefinition, owner: str) -> dict[str, Any]:
    out: dict[str, Any] = {"name": slot.name}
    for field in SLOT_VALUES:
        value = getattr(slot, field, None)
        if value is None:
            continue
        if field == "range":
            value = _range(view, str(value), owner)
        # `rank` stays a number (the display order, DM-98, T-4672), a flag a boolean.
        out[field] = value if isinstance(value, (bool, int)) else str(value)
    unit = getattr(slot, "unit", None)
    if unit is not None:
        fields = {
            "ucum_code": getattr(unit, "ucum_code", None),
            "symbol": getattr(unit, "symbol", None),
            "exact_mappings": [str(m) for m in getattr(unit, "exact_mappings", None) or []],
        }
        present = {k: v for k, v in fields.items() if v}
        if present:
            out["unit"] = present
    annotations = _annotations(slot)
    if annotations:
        out["annotations"] = annotations
    return out


def _usage(base: dict[str, Any], induced: dict[str, Any]) -> dict[str, Any]:
    """The metaslots `induced` states differently from `base`; empty when they agree."""
    diff = {k: v for k, v in induced.items() if k not in ("name", "annotations") and base.get(k) != v}
    changed = {
        tag: a for tag, a in induced.get("annotations", {}).items() if base.get("annotations", {}).get(tag) != a
    }
    if changed:
        diff["annotations"] = changed
    return {"name": induced["name"], **diff} if diff else {}


def compile_broker_schema(source: str | Path) -> dict[str, Any]:
    """The broker schema of one LinkML model, imports merged."""
    view = load(source)
    schema = view.schema
    default_range = schema.default_range or "string"

    slots: dict[str, dict[str, Any]] = {}
    for name, slot in sorted(view.all_slots().items()):
        declared = _slot(view, slot, f"slot '{name}'")
        declared.setdefault("range", _range(view, str(default_range), f"slot '{name}'"))
        slots[name] = declared

    classes: dict[str, dict[str, Any]] = {}
    for name, cls in sorted(view.all_classes().items()):
        names = list(view.class_slots(name))
        usage = {}
        for slot_name in names:
            induced = _slot(view, view.induced_slot(slot_name, name), f"class '{name}' slot '{slot_name}'")
            differs = _usage(slots[slot_name], induced)
            if differs:
                usage[slot_name] = differs
        entry: dict[str, Any] = {"name": name, "class_uri": str(cls.class_uri or name)}
        if cls.is_a:
            entry["is_a"] = str(cls.is_a)
        entry["slots"] = names
        if usage:
            entry["slot_usage"] = usage
        annotations = _annotations(cls)
        if annotations:
            entry["annotations"] = annotations
        if cls.description:
            entry["description"] = str(cls.description)
        classes[name] = entry

    enums: dict[str, dict[str, Any]] = {}
    for name, enum in sorted(view.all_enums().items()):
        values: dict[str, dict[str, str]] = {}
        for text, pv in (enum.permissible_values or {}).items():
            value = {"text": str(text)}
            if pv.description:
                value["description"] = str(pv.description)
            if pv.meaning:
                value["meaning"] = str(pv.meaning)
            values[str(text)] = value
        entry = {"name": name, "permissible_values": values}
        if enum.description:
            entry["description"] = str(enum.description)
        enums[name] = entry

    out: dict[str, Any] = {
        "id": str(schema.id),
        "name": str(schema.name),
        "prefixes": {
            str(k): {"prefix_prefix": str(k), "prefix_reference": str(p.prefix_reference)}
            for k, p in sorted(schema.prefixes.items())
        },
    }
    if schema.version:
        out["version"] = str(schema.version)
    if schema.default_prefix:
        out["default_prefix"] = str(schema.default_prefix)
    out.update(classes=classes, slots=slots, enums=enums)
    return out


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("schema", help="the .linkml.yaml file")
    parser.add_argument("-o", "--output", help="write here instead of stdout")
    args = parser.parse_args(argv)
    try:
        text = json.dumps(compile_broker_schema(args.schema), indent=2, ensure_ascii=False) + "\n"
    except ModelError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1
    if args.output:
        Path(args.output).write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
