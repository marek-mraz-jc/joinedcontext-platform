"""LinkML-Map to the gateway's mapping IR (T-0173, DM-51, DM-52, EP-54).

One specification, two artifacts. The Bloblang of `compile_bloblang` runs in Bento and may
use everything DM-36 allows; this one is what the Context Gateway executes when a
`ContextSourceRegistration` or an Endpoint carries a `mappingRef`, and it is deliberately
smaller, because the gateway does not only translate answers. It also rewrites the query on
its way out: `attrs`, `q`, `geoQ` and the enum values inside them are written in the target
model and have to reach the source in the source's own names. That is an inverse, and only
an invertible derivation has one.

So the IR carries the invertible subset of DM-51 with both directions precomputed, and marks
everything else non-filterable rather than dropping it: an `expr` slot is still served, from
the expression tree this document carries, it just cannot appear in a filter; and a `native`
block is refused outright because there is nothing to invert and replicate mode is where such
a mapping belongs.

The IR is JSON with its own version, because the gateway reads artifacts a repository
committed at some earlier version of these tools (DM-52).
"""

from __future__ import annotations

from typing import Any

from linkml_map.datamodel.transformer_model import ClassDerivation, SlotDerivation
from linkml_runtime import SchemaView

from common import ModelError, load
from compile_bloblang import (
    CASTS,
    class_derivation,
    expression_tree,
    linear_conversion,
    refuse_unsupported_slot,
    slot_derivations,
    specification,
)

#: Schema version of the IR document. The gateway reads committed artifacts, so it has to be
#: able to tell an IR it understands from one a newer Model Tools wrote.
IR_VERSION = 2


def compile_mapping_ir(
    transformation: Any, *, source: str | None = None, native: Any = ()
) -> dict[str, Any]:
    """Compile one Mapping to the gateway mapping IR (DM-51, DM-52).

    `native` is `spec.native`; a Mapping carrying one has no live translation at all, so it
    is refused here rather than served as a mapping that silently ignores a block.
    """
    if list(native or ()):
        slots = ", ".join(sorted(str(entry.get("targetSlot") or entry.get("target_slot"))
                                 for entry in native))
        raise ModelError(
            f"native block(s) for {slots}: a native block has no inverse, so the gateway "
            f"cannot rewrite a query through it. Use replicate mode, where a Bento pipeline "
            f"applies the compiled Bloblang and the data lands locally (DM-51)."
        )

    spec = specification(transformation)
    target_class, derivation = class_derivation(spec)
    view = load(source) if source else None

    slots = [
        _slot(name, slot, derivation, view)
        for name, slot in slot_derivations(derivation).items()
    ]
    return {
        "version": IR_VERSION,
        "sourceClass": derivation.populated_from or "",
        "targetClass": target_class,
        "slots": [slot for slot in slots if slot is not None],
    }


def _slot(
    name: str, slot: SlotDerivation, derivation: ClassDerivation, view: SchemaView | None
) -> dict[str, Any] | None:
    """One IR entry, or None where the slot is not served at all."""
    refuse_unsupported_slot(name, slot)
    if slot.hide:
        return None
    if slot.value is not None:
        # A constant depends on no source slot, so it is served and never filtered: a filter
        # on it says nothing about which source entities to ask for.
        return {"target": name, "kind": "constant", "value": slot.value, "filterable": False}
    if slot.expr:
        # DM-51: returned, but not filterable. The tree is what makes the first half true —
        # without it the gateway has nothing to compute and the slot is silently absent — and
        # it is the same tree the Bloblang was rendered from, so the two executors cannot be
        # given different expressions. Filtering stays refused: an expression has no inverse.
        return {
            "target": name,
            "kind": "expr",
            "filterable": False,
            "expression": expression_tree(name, slot.expr),
        }

    source_slot = _source_slot(name, slot, derivation)
    if slot.unit_conversion:
        factor, offset = linear_conversion(name, slot, derivation, view)
        return {
            "target": name, "source": source_slot, "kind": "unitConversion",
            "factor": factor, "offset": offset, "filterable": True,
        }
    if slot.value_mappings:
        forward = _forward(name, slot)
        return {
            "target": name, "source": source_slot, "kind": "valueMappings",
            "forward": forward, "inverse": _inverse(name, forward), "filterable": True,
        }
    if slot.range:
        if slot.range not in CASTS:
            raise ModelError(
                f"target slot '{name}': range '{slot.range}' is not a cast this compiler "
                f"supports ({', '.join(CASTS)})"
            )
        return {
            "target": name, "source": source_slot, "kind": "cast",
            "range": slot.range, "filterable": True,
        }
    return {"target": name, "source": source_slot, "kind": "rename", "filterable": True}


def _source_slot(name: str, slot: SlotDerivation, derivation: ClassDerivation) -> str:
    """The source slot an IR entry reads, in the source model's own name."""
    populated_from = slot.populated_from or name
    if "." not in populated_from:
        return populated_from
    table, _, field = populated_from.partition(".")
    if table == (derivation.populated_from or ""):
        return field
    raise ModelError(
        f"target slot '{name}': populated_from '{populated_from}' names the table '{table}', "
        f"which the source class '{derivation.populated_from}' is not. The gateway asks one "
        f"context source for one entity and has nothing to join against."
    )


def _forward(name: str, slot: SlotDerivation) -> dict[str, Any]:
    mapping: dict[str, Any] = {}
    for key, mapped in slot.value_mappings.items():
        value = getattr(mapped, "value", mapped)
        if value is None:
            raise ModelError(
                f"target slot '{name}': value_mappings entry '{key}' maps to nothing"
            )
        mapping[str(key)] = value
    return mapping


def _inverse(name: str, forward: dict[str, Any]) -> dict[str, Any]:
    """The value mapping read backwards, refusing a table that has no single answer.

    Two source values collapsing onto one target value is a perfectly good forward mapping
    and has no inverse: a query for that target value cannot say which source value to ask
    for. Live translation is the one place where that matters, so it is caught here rather
    than producing an IR the gateway would have to guess with (DM-51).
    """
    inverse: dict[str, Any] = {}
    for source_value, target_value in forward.items():
        key = str(target_value)
        if key in inverse:
            raise ModelError(
                f"target slot '{name}': value_mappings sends both '{inverse[key]}' and "
                f"'{source_value}' to '{key}', so a query for '{key}' has no single source "
                f"value to ask for. Live translation needs an invertible mapping (DM-51); "
                f"use replicate mode for this Mapping."
            )
        inverse[key] = source_value
    return inverse
