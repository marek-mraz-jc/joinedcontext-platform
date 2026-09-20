"""LinkML-Map to Bloblang, the compiler behind every Mapping (T-0172, DM-33, DM-35…DM-37).

Bento is the only transformation runtime and Bloblang the only runtime language, so a
`kind: Mapping` is only ever executed as the Bloblang this module emits; the
`TransformationSpecification` itself is never interpreted at runtime (DM-37). The compiler
is small because the source language is small, and it is deliberately *closed*: a construct
it does not understand fails the compilation with a message naming the derivation, because a
silent fallback is a mapping that runs and quietly produces the wrong entity (DM-36).

Alongside the mapping it returns a compile report, one row per derivation, saying which
Bloblang line each target slot came from and whether the compiler checked it. A `native:
bloblang` block is copied through and reported as unchecked, which is where the schema
guarantee stops and where the reviewer is meant to look (DM-38).

Everything here agrees with `linkml_map`'s own reference engine on purpose, down to the
truncation direction of an integer cast, because `test_mapping_parity` runs both over the
same golden tests and any disagreement is a bug in one of them (DM-39).
"""

from __future__ import annotations

import ast
import json
from copy import deepcopy
from typing import Any, Iterable

from linkml_map.datamodel.transformer_model import (
    ClassDerivation,
    SlotDerivation,
    TransformationSpecification,
)
from linkml_map.functions.unit_conversion import UnitSystem, convert_units
from linkml_map.spec_normalizer import normalize_spec
from linkml_runtime import SchemaView

from common import ModelError, load

#: Fields of a `ClassDerivation` the compiler acts on. `joins` is absent on purpose: the
#: reference engine resolves one through an object or lookup index, and a Bloblang mapping
#: sees one message with no index to consult, so compiling it would produce a mapping that
#: cannot mean what the specification says.
CLASS_FIELDS = frozenset({"name", "populated_from", "slot_derivations"})

#: Fields of a `SlotDerivation` the compiler acts on, which is exactly the list of DM-36.
SLOT_FIELDS = frozenset(
    {"name", "populated_from", "expr", "value", "value_mappings", "unit_conversion", "range",
     "hide", "derived_from"}
)

#: Target ranges the compiler casts to, and the Bloblang that matches what Python's own
#: `int()`, `float()` and `str()` do in the reference engine. `boolean` is missing on
#: purpose: Python casts by truthiness, so `bool("false")` is true, while Bloblang's `.bool()`
#: parses the text. A compiled mapping would disagree with the specification exactly where a
#: reviewer would never look.
CASTS = ("integer", "float", "string")


class BloblangArtifact:
    """The compiled mapping and the report that says where every line came from (DM-35)."""

    def __init__(self, mapping: str, report: list[dict[str, Any]], source: str, target: str):
        self.mapping = mapping
        self.report = report
        self.source_class = source
        self.target_class = target

    def as_dict(self) -> dict[str, Any]:
        return {
            "mapping": self.mapping,
            "report": self.report,
            "sourceClass": self.source_class,
            "targetClass": self.target_class,
        }


def specification(transformation: Any) -> TransformationSpecification:
    """The `TransformationSpecification` of a Mapping, parsed once for every consumer.

    `spec.transformation` is carried verbatim in the manifest (DM-33), so this is where a
    malformed one turns into a message the person editing it can act on rather than a
    pydantic traceback.
    """
    if isinstance(transformation, TransformationSpecification):
        return transformation
    if not isinstance(transformation, dict):
        raise ModelError("spec.transformation must be a LinkML-Map TransformationSpecification")
    # `normalize_spec` is the library's own loader step: it turns the inlined-as-dict form
    # LinkML-Map documents are written in into the shape the pydantic model expects, injects
    # each derivation's `name` from its key and migrates the deprecated spellings. Running it
    # here is what makes this compiler and the reference engine read the same document; it
    # mutates what it is given, so the manifest's own dict is left alone.
    document = deepcopy(transformation)
    try:
        normalize_spec(document)
        return TransformationSpecification(**document)
    except Exception as error:  # noqa: BLE001 - pydantic and linkml-map raise their own
        raise ModelError(f"spec.transformation is not a valid LinkML-Map document: {error}")


def compile_bloblang(
    transformation: Any,
    *,
    source: str | None = None,
    native: Iterable[dict[str, Any]] = (),
) -> BloblangArtifact:
    """Compile one Mapping to Bloblang and its compile report (DM-35, DM-36).

    `source` is the source LinkML schema, needed only where a `unit_conversion` leaves its
    source unit to the model; without it such a derivation is refused rather than guessed.
    `native` carries `spec.native` (DM-38).
    """
    spec = specification(transformation)
    target, derivation = class_derivation(spec)
    view = load(source) if source else None

    natives = _native_blocks(native)
    lines: list[str] = []
    report: list[dict[str, Any]] = []

    for name, slot in slot_derivations(derivation).items():
        if name in natives:
            block = natives.pop(name)
            lines.append(block)
            report.append(_row(name, "native", block, checked=False))
            continue
        refuse_unsupported_slot(name, slot)
        expression, kind = _slot(name, slot, derivation, view)
        if expression is None:
            report.append(_row(name, kind, None, checked=True))
            continue
        line = f"root.{name} = {expression}"
        lines.append(line)
        report.append(_row(name, kind, line, checked=True))

    if natives:
        listed = ", ".join(sorted(natives))
        raise ModelError(
            f"native block(s) for {listed}: no slot_derivation of target class "
            f"'{target}' carries that name, so the block would never run (DM-38)"
        )
    if not lines:
        raise ModelError(f"target class '{target}' derives no slot, so the mapping is empty")

    mapping = "\n".join(["root = {}", *lines, ""])
    return BloblangArtifact(mapping, report, derivation.populated_from or "", target)


# --- the specification ----------------------------------------------------------------


def class_derivation(spec: TransformationSpecification) -> tuple[str, ClassDerivation]:
    """The single class a Mapping produces.

    A `kind: Mapping` names one source model and one target model and a pipeline injects one
    `mapping` processor, so a specification deriving two classes has no single answer to
    "what does this message become".
    """
    # Normalization leaves class derivations as a list carrying their own `name`, and slot
    # derivations as a dict keyed by it. Neither shape is the one the document was written in.
    derivations = list(spec.class_derivations or [])
    if not derivations:
        raise ModelError("spec.transformation declares no class_derivations")
    if len(derivations) > 1:
        listed = ", ".join(sorted(str(d.name) for d in derivations))
        raise ModelError(
            f"spec.transformation derives {len(derivations)} classes ({listed}). A Mapping "
            f"produces one target class; split it into one Mapping per target."
        )
    derivation = derivations[0]
    name = str(derivation.name)
    _refuse_unsupported_class(name, derivation)
    return name, derivation


def _refuse_unsupported_class(name: str, derivation: ClassDerivation) -> None:
    if "joins" in derivation.model_fields_set and derivation.joins:
        raise ModelError(
            f"class derivation '{name}': `joins` resolves a row from another table, and a "
            f"Bento mapping sees one message with no table to join against. Join in the "
            f"pipeline before the mapping processor, or carry the referenced object inline."
        )
    extra = sorted(derivation.model_fields_set - CLASS_FIELDS - {"joins"})
    if extra:
        raise ModelError(
            f"class derivation '{name}': {', '.join(extra)} is not a construct this compiler "
            f"supports (DM-36)"
        )


def slot_derivations(derivation: ClassDerivation) -> dict[str, SlotDerivation]:
    slots = dict(derivation.slot_derivations or {})
    if not slots:
        raise ModelError(f"class derivation '{derivation.name}' declares no slot_derivations")
    return slots


def refuse_unsupported_slot(name: str, slot: SlotDerivation) -> None:
    extra = sorted(slot.model_fields_set - SLOT_FIELDS)
    if extra:
        raise ModelError(
            f"target slot '{name}': {', '.join(extra)} is not a construct this compiler "
            f"supports (DM-36). Express it in the pipeline's own Bloblang, or attach a "
            f"`native: bloblang` block to this slot and accept the raised review lane."
        )


# --- one slot -------------------------------------------------------------------------


def _slot(
    name: str, slot: SlotDerivation, derivation: ClassDerivation, view: SchemaView | None
) -> tuple[str | None, str]:
    """The Bloblang for one target slot, and the name of the derivation it came from.

    A `None` expression means the slot is deliberately absent from the output. The order of
    the branches is the reference engine's own, so the two agree on a derivation that sets
    more than one field.
    """
    if slot.hide:
        return None, "hide"
    if slot.value is not None:
        return _literal(slot.value), "value"
    if slot.unit_conversion:
        return _unit_conversion(name, slot, derivation, view), "unit_conversion"
    if slot.expr:
        return _cast(_expression(name, slot.expr), name, slot.range), "expr"
    if slot.populated_from:
        read = _read(name, slot.populated_from, derivation)
    elif slot.value_mappings or slot.range:
        # The reference engine falls back to the target slot's own name.
        read = _read(name, name, derivation)
    elif slot.derived_from:
        # Provenance only: it records which source slots a value came from and populates
        # nothing on its own.
        return None, "derived_from"
    else:
        read = _read(name, name, derivation)
    if slot.value_mappings:
        return _cast(_value_mappings(name, slot, read), name, slot.range), "value_mappings"
    return _cast(read, name, slot.range), "populated_from"


def _read(name: str, populated_from: str, derivation: ClassDerivation) -> str:
    """The Bloblang path a `populated_from` reads.

    LinkML-Map reads a dotted `populated_from` as `table.field`, not as a nested path, and
    resolves anything but the source class through a join or a foreign key. Only the two
    forms a single message can answer are compiled: the bare slot, and the slot qualified by
    the source class the derivation is populated from.
    """
    if "." not in populated_from:
        return f"this.{populated_from}"
    table, _, field = populated_from.partition(".")
    if table == (derivation.populated_from or ""):
        return f"this.{field}"
    raise ModelError(
        f"target slot '{name}': populated_from '{populated_from}' names the table '{table}', "
        f"which the source class '{derivation.populated_from}' is not. Resolving it needs a "
        f"lookup across messages, which a Bento mapping cannot do."
    )


def _literal(value: Any) -> str:
    return json.dumps(value)


def _value_mappings(name: str, slot: SlotDerivation, read: str) -> str:
    """`value_mappings` as a Bloblang `match`.

    The default arm throws rather than passing the value through or dropping it: an enum
    value nobody mapped is a data error, and DM-36 forbids a silent fallback.
    """
    arms = []
    for source_value, mapped in slot.value_mappings.items():
        target = getattr(mapped, "value", mapped)
        if target is None:
            raise ModelError(
                f"target slot '{name}': value_mappings entry '{source_value}' maps to nothing"
            )
        arms.append(f"  {_literal(source_value)} => {_literal(target)}")
    message = f"{name}: no value_mappings entry for the source value"
    arms.append(f"  _ => throw({_literal(message)})")
    return "match {} {{\n{}\n}}".format(read, "\n".join(arms))


def linear_conversion(
    name: str, slot: SlotDerivation, derivation: ClassDerivation, view: SchemaView | None
) -> tuple[float, float]:
    """The factor and offset of a `unit_conversion`, computed once for both artifacts.

    Neither Bloblang nor the gateway has a unit registry, so the conversion has to become
    arithmetic. Asking the reference engine's own converter for the value at 0 and at 1 gives
    the affine pair, and a third point proves the conversion really is affine: a logarithmic
    or otherwise non-linear unit produces numbers a factor and an offset cannot reproduce,
    and is refused rather than silently approximated (DM-51 admits only the linear subset).

    Both compilers call this, which is what makes the Bloblang and the IR agree on a number
    rather than agreeing by inspection (DM-52).
    """
    configuration = slot.unit_conversion
    unsupported = sorted(
        configuration.model_fields_set - {"target_unit", "target_unit_scheme", "source_unit"}
    )
    if unsupported:
        raise ModelError(
            f"target slot '{name}': unit_conversion {', '.join(unsupported)} is not a "
            f"construct this compiler supports (DM-36)"
        )
    target_unit = configuration.target_unit
    if not target_unit:
        raise ModelError(f"target slot '{name}': unit_conversion declares no target_unit")
    source_unit = configuration.source_unit or declared_unit(name, slot, derivation, view)
    system = UnitSystem(configuration.target_unit_scheme) if configuration.target_unit_scheme else UnitSystem.UCUM

    try:
        at_zero = float(convert_units(0, source_unit, target_unit, system=system))
        at_one = float(convert_units(1, source_unit, target_unit, system=system))
        at_seven = float(convert_units(7, source_unit, target_unit, system=system))
    except Exception as error:  # noqa: BLE001 - pint and ucumvert raise their own
        raise ModelError(
            f"target slot '{name}': cannot convert '{source_unit}' to '{target_unit}': {error}"
        )
    factor = at_one - at_zero
    if abs(at_seven - (7 * factor + at_zero)) > 1e-9:
        raise ModelError(
            f"target slot '{name}': the conversion from '{source_unit}' to '{target_unit}' is "
            f"not linear, and only a linear conversion compiles to arithmetic (DM-51)"
        )
    return factor, at_zero


def _unit_conversion(
    name: str, slot: SlotDerivation, derivation: ClassDerivation, view: SchemaView | None
) -> str:
    """A linear unit conversion as Bloblang arithmetic."""
    factor, offset = linear_conversion(name, slot, derivation, view)
    read = _read(name, slot.populated_from or name, derivation)
    expression = f"({read}.number() * {factor!r})"
    if offset:
        expression = f"({expression} + {offset!r})"
    return _cast(expression, name, slot.range)


def declared_unit(
    name: str, slot: SlotDerivation, derivation: ClassDerivation, view: SchemaView | None
) -> str:
    """The UCUM code the source model puts on the slot the conversion reads (DM-06)."""
    if view is None:
        raise ModelError(
            f"target slot '{name}': unit_conversion names no source_unit and the source "
            f"schema was not supplied, so there is nothing to read the unit from"
        )
    source_class = derivation.populated_from
    source_slot = (slot.populated_from or name).rpartition(".")[2]
    try:
        induced = view.induced_slot(source_slot, source_class)
    except Exception:  # noqa: BLE001 - linkml raises on an unknown class or slot
        induced = None
    code = getattr(getattr(induced, "unit", None), "ucum_code", None)
    if not code:
        raise ModelError(
            f"target slot '{name}': slot '{source_slot}' of '{source_class}' declares no "
            f"unit, so a unit_conversion from it has no source unit (DM-34)"
        )
    return str(code)


def _cast(expression: str, name: str, target_range: str | None) -> str:
    """A target `range` as the Bloblang that matches the reference engine's Python cast."""
    if not target_range:
        return expression
    if target_range not in CASTS:
        raise ModelError(
            f"target slot '{name}': range '{target_range}' is not a cast this compiler "
            f"supports ({', '.join(CASTS)})"
        )
    if target_range == "string":
        return f"{expression}.string()"
    if target_range == "float":
        return f"{expression}.number()"
    return _integer(expression, name)


def _integer(expression: str, name: str) -> str:
    """Python's `int()` as Bloblang, which is not `.number().floor()` (DM-39).

    The reference engine casts with `int()`, and `int()` does two different things. Given a
    number it truncates towards zero, so the direction has to be chosen by the sign: Bloblang
    `floor()` rounds -4.7 to -5 where Python gives -4. Given a string it parses an integer
    literal and *refuses* anything else, so `int("-4.7")` raises where Bloblang's `.number()`
    happily returns -4.7 and truncates. The golden runner caught that second one: a CSV column
    of decimals cast to integer would have been silently truncated by the compiled mapping and
    rejected by the specification it claims to implement.

    The arms are written subjectless (`match { <condition> => … }`) on purpose: a `match` with
    a subject rebinds `this` to that subject inside every arm, and the arms need the document.
    """
    text = f'{expression}.re_match("^[+-]?[0-9]+$")'
    refusal = _literal(f"{name}: integer cast of a value that is not an integer")
    return (
        f"(match {{\n"
        f"  {expression}.type() == \"string\" => "
        f"if {text} {{ {expression}.number() }} else {{ throw({refusal}) }}\n"
        f"  _ => if {expression}.number() < 0 {{ {expression}.number().ceil() }} "
        f"else {{ {expression}.number().floor() }}\n"
        f"}})"
    )


# --- expressions ----------------------------------------------------------------------

BINARY = {ast.Add: "+", ast.Sub: "-", ast.Mult: "*", ast.Div: "/"}
COMPARISON = {ast.Eq: "==", ast.NotEq: "!=", ast.Lt: "<", ast.LtE: "<=", ast.Gt: ">", ast.GtE: ">="}
BOOLEAN = {ast.And: "and", ast.Or: "or"}
#: Bloblang spells the two boolean operators differently from the tree the IR carries.
BLOBLANG_BOOLEAN = {"and": "&&", "or": "||"}


def expression_tree(name: str, expr: str) -> dict[str, Any]:
    """One `expr` derivation as the typed tree both artifacts are rendered from (DM-51, DM-52).

    The expression is parsed rather than rewritten with string surgery, and only the node
    types DM-36 names survive the walk: arithmetic, concatenation and comparison over source
    slots and literals. Everything else — a call, a subscript, a comprehension, an attribute
    on a name — is refused by node type, which is what makes "rejects arbitrary code" a
    property of the parser and not of a blocklist somebody has to keep current.

    The tree is the acceptance surface for both compilers. Bento gets Bloblang rendered from
    it, and the gateway gets it as it stands inside the mapping IR, so the two executors
    cannot be handed expressions the other would refuse and no second parser exists to drift.
    """
    text = expr.replace("{", "").replace("}", "")
    try:
        tree = ast.parse(text, mode="eval")
    except SyntaxError as error:
        raise ModelError(f"target slot '{name}': expr does not parse: {error.msg}")
    return _node(name, tree.body)


def _node(name: str, node: ast.AST) -> dict[str, Any]:
    if isinstance(node, ast.Name):
        return {"slot": node.id}
    if isinstance(node, ast.Constant):
        if isinstance(node.value, (str, int, float, bool)) or node.value is None:
            return {"const": node.value}
    if isinstance(node, ast.BinOp) and type(node.op) in BINARY:
        return {
            "binary": BINARY[type(node.op)],
            "left": _node(name, node.left),
            "right": _node(name, node.right),
        }
    if isinstance(node, ast.UnaryOp):
        if isinstance(node.op, ast.USub):
            return {"unary": "-", "operand": _node(name, node.operand)}
        if isinstance(node.op, ast.Not):
            return {"unary": "not", "operand": _node(name, node.operand)}
    if isinstance(node, ast.BoolOp) and type(node.op) in BOOLEAN:
        return {
            "boolean": BOOLEAN[type(node.op)],
            "operands": [_node(name, value) for value in node.values],
        }
    if isinstance(node, ast.Compare):
        if len(node.ops) != 1:
            raise ModelError(
                f"target slot '{name}': a chained comparison is not a construct this "
                f"compiler supports (DM-36); write it as two comparisons joined by `and`"
            )
        if type(node.ops[0]) in COMPARISON:
            return {
                "compare": COMPARISON[type(node.ops[0])],
                "left": _node(name, node.left),
                "right": _node(name, node.comparators[0]),
            }
    raise ModelError(
        f"target slot '{name}': expr uses {type(node).__name__}, which is not arithmetic, "
        f"concatenation or comparison (DM-36). Use value_mappings for an enum, or attach a "
        f"`native: bloblang` block and accept the raised review lane."
    )


def _expression(name: str, expr: str) -> str:
    """The Bloblang of one `expr` derivation, rendered from the tree the IR also carries."""
    return _bloblang(expression_tree(name, expr))


def _bloblang(node: dict[str, Any]) -> str:
    """One expression node as Bloblang."""
    if "slot" in node:
        return f"this.{node['slot']}"
    if "const" in node:
        return _literal(node["const"])
    if "binary" in node:
        return f"({_bloblang(node['left'])} {node['binary']} {_bloblang(node['right'])})"
    if "compare" in node:
        return f"({_bloblang(node['left'])} {node['compare']} {_bloblang(node['right'])})"
    if "boolean" in node:
        operator = BLOBLANG_BOOLEAN[node["boolean"]]
        return "(" + f" {operator} ".join(_bloblang(value) for value in node["operands"]) + ")"
    operand = _bloblang(node["operand"])
    return f"(-{operand})" if node["unary"] == "-" else f"(!{operand})"


# --- native blocks --------------------------------------------------------------------


def _native_blocks(native: Iterable[dict[str, Any]]) -> dict[str, str]:
    """`spec.native` keyed by the target slot each block is attached to (DM-38)."""
    blocks: dict[str, str] = {}
    for entry in native or ():
        slot = entry.get("targetSlot") or entry.get("target_slot")
        language = entry.get("language", "bloblang")
        code = entry.get("source")
        if not slot or not code:
            raise ModelError("a native block needs a targetSlot and a source (DM-38)")
        if language != "bloblang":
            raise ModelError(
                f"native block for '{slot}': language '{language}' is not bloblang, and "
                f"Bloblang is the only runtime language (DM-37)"
            )
        if slot in blocks:
            raise ModelError(f"native block for '{slot}' is declared twice")
        blocks[slot] = code.strip()
    return blocks


def _row(name: str, derivation: str, line: str | None, *, checked: bool) -> dict[str, Any]:
    """One compile-report row: the derivation, its Bloblang, and whether it was checked."""
    return {"targetSlot": name, "derivation": derivation, "bloblang": line, "checked": checked}
