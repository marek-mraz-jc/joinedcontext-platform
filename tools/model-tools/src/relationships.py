"""Relationships between classes, strict like foreign keys (DM-64…DM-69, Architecture/11 §1.2).

A relationship is a pair of slots naming each other in `inverse`. The cardinality is the two
`multivalued` flags read from the source end, which is the end carrying `on_delete` or, when
neither does, the end declared first. Exactly one end is stored, the "many" side as a foreign key
would be (DM-67); the other is a query and never appears on an entity.

Every generator refuses a model that breaks a rule, and `/generate` answers the broken rules one
per entry, `{path}: {rule}: {message}`, with the rule identifiers the editor and the gateway use:
the Portal's save route refuses on exactly this list (DM-68).
"""

from __future__ import annotations

from dataclasses import dataclass

from linkml_runtime import SchemaView
from linkml_runtime.linkml_model.meta import SlotDefinition

from common import annotation_value, ngsi_ld_kind

ON_DELETE_RULES = ("restrict", "cascade", "set-null")

#: The range of an external reference: the id of an entity outside the model (DM-69).
EXTERNAL_RANGE = "uriorcurie"


@dataclass(frozen=True)
class End:
    """One end of a relationship: the class holding the slot, the slot, and its flags."""

    cls: str
    slot: str
    multivalued: bool
    required: bool


@dataclass(frozen=True)
class Relationship:
    source: End
    target: End
    cardinality: str
    on_delete: str
    #: "source" or "target": the end whose entities hold the NGSI-LD Relationship (DM-67).
    stored: str

    @property
    def stored_end(self) -> End:
        return self.source if self.stored == "source" else self.target

    @property
    def computed_end(self) -> End:
        return self.target if self.stored == "source" else self.source


def cardinality_of(source_many: bool, target_many: bool) -> str:
    """The cardinality two `multivalued` flags make, read from the source (DM-65)."""
    if source_many:
        return "many-to-many" if target_many else "one-to-many"
    return "many-to-one" if target_many else "one-to-one"


def stored_end(cardinality: str) -> str:
    """Where a foreign key would be: the "many" side, or the source of 1:1 and N:M (DM-67)."""
    return "target" if cardinality == "one-to-many" else "source"


@dataclass(frozen=True)
class _Placed:
    cls: str
    slot: SlotDefinition
    path: str
    order: int
    #: The range as written, None where the model writes none (DM-69).
    range: str | None


def _declared_range(view: SchemaView, cls, name: str) -> str | None:
    """The range the model writes for a slot of a class, before `default_range` fills it in.

    An induced slot takes the model's `default_range`, so a Relationship written without a range,
    as Smart Data Models writes one, would read as a string; it is an external reference (DM-69).
    """
    usage = (cls.slot_usage or {}).get(name)
    if usage is not None and getattr(usage, "range", None):
        return usage.range
    attribute = (cls.attributes or {}).get(name)
    if attribute is not None:
        return attribute.range
    declared = view.get_slot(name)
    return None if declared is None else declared.range


def _placed(view: SchemaView) -> tuple[list[_Placed], dict[str, list[str]]]:
    """Every slot where a class declares it (listed or inline), and who lists each slot."""
    order = {name: index for index, name in enumerate(view.schema.slots or {})}
    inline_order = len(order)
    placed: list[_Placed] = []
    listed_by: dict[str, list[str]] = {}
    for cls in view.all_classes().values():
        for name in cls.slots or []:
            listed_by.setdefault(name, []).append(cls.name)
            placed.append(
                _Placed(cls.name, view.induced_slot(name, cls.name), f"slots.{name}", order.get(name, 0), _declared_range(view, cls, name))
            )
        for name in cls.attributes or {}:
            placed.append(
                _Placed(
                    cls.name,
                    view.induced_slot(name, cls.name),
                    f"classes.{cls.name}.attributes.{name}",
                    inline_order,
                    _declared_range(view, cls, name),
                )
            )
            inline_order += 1
    return placed, listed_by


def analyse(view: SchemaView) -> tuple[list[Relationship], list[str]]:
    """Every relationship of the merged model, and every rule it breaks as `{path}: {rule}: {message}`."""
    classes = set(view.all_classes())
    primitives = set(view.all_types()) | set(view.all_enums())
    placed, listed_by = _placed(view)
    by_end = {(one.cls, one.slot.name): one for one in placed}

    problems: list[str] = []
    relationships: list[Relationship] = []
    seen: set[tuple[str, str]] = set()

    def problem(path: str, rule: str, message: str) -> None:
        entry = f"{path}: {rule}: {message}"
        if entry not in problems:
            problems.append(entry)

    for end in placed:
        slot, owner, path = end.slot, end.cls, end.path
        kind = ngsi_ld_kind(slot)
        target = end.range
        class_range = target in classes
        if kind != "Relationship":
            # A nested value's class describes its shape (the importer's `address`), no entity.
            if class_range and kind != "JsonProperty" and slot.inlined is not True:
                problem(
                    path,
                    "class-range-not-relationship",
                    f"{slot.name} on {owner} points at the class {target} but is a {kind}; "
                    "a slot whose range is a class is a Relationship",
                )
            continue
        if target is None or target == EXTERNAL_RANGE:
            if slot.inverse:
                problem(
                    path,
                    "range-not-a-class",
                    f"{slot.name} on {owner} names the inverse {slot.inverse} but its range is no class; "
                    "name the class it points at",
                )
            continue
        if target in primitives:
            problem(
                path,
                "primitive-range",
                f"{slot.name} on {owner} is a Relationship with the range {target}; a Relationship points "
                "at a class, or at uriorcurie for an entity outside the model",
            )
            continue
        if not class_range:
            problem(path, "range-not-a-class", f"{slot.name} on {owner} points at {target}, which is no class of this model or of its imports")
            continue
        on_delete = annotation_value(slot, "on_delete")
        if on_delete is not None and on_delete not in ON_DELETE_RULES:
            problem(path, "on-delete-unknown", f"{slot.name} on {owner} has on_delete '{on_delete}'; it is one of {', '.join(ON_DELETE_RULES)}")
        owners = listed_by.get(slot.name, [])
        if path.startswith("slots.") and len(owners) > 1:
            problem(
                path,
                "slot-in-two-relationships",
                f"{slot.name} is used by {' and '.join(owners)}, so it would be an end of two relationships; "
                "give each class a slot of its own",
            )
            continue
        if not slot.inverse:
            problem(path, "inverse-missing", f"{slot.name} ({owner} → {target}) names no inverse; add the slot on {target} that points back")
            continue
        other = by_end.get((target, slot.inverse))
        if other is None:
            problem(path, "inverse-missing", f"{slot.name} names the inverse {slot.inverse}, which {target} does not have")
            continue
        if other.cls == owner and other.slot.name == slot.name:
            problem(path, "inverse-not-reciprocal", f"{slot.name} names itself as its inverse; the other end is a slot of its own")
            continue
        if ngsi_ld_kind(other.slot) != "Relationship" or other.range != owner or other.slot.inverse != slot.name:
            problem(
                path,
                "inverse-not-reciprocal",
                f"{slot.name} ({owner} → {target}) names {target}.{slot.inverse} as its inverse, which does not point back at {owner}.{slot.name}",
            )
            continue
        here, there = f"{owner}.{slot.name}", f"{target}.{other.slot.name}"
        key = (min(here, there), max(here, there))
        if key in seen:
            continue
        seen.add(key)
        other_delete = annotation_value(other.slot, "on_delete")
        if on_delete is not None and other_delete is not None:
            problem(path, "on-delete-on-both-ends", f"both {slot.name} and {other.slot.name} carry on_delete; it goes on the source end only")
            continue
        this_is_source = on_delete is not None or (other_delete is None and end.order <= other.order)
        source, tgt = (end, other) if this_is_source else (other, end)
        cardinality = cardinality_of(bool(source.slot.multivalued), bool(tgt.slot.multivalued))
        stored = stored_end(cardinality)
        computed = tgt if stored == "source" else source
        if computed.slot.required:
            problem(
                computed.path,
                "required-on-computed-end",
                f"{computed.slot.name} on {computed.cls} is computed on read and cannot be required; make the stored end required instead",
            )
        rule = annotation_value(source.slot, "on_delete") or "restrict"
        relationships.append(
            Relationship(
                source=End(source.cls, source.slot.name, bool(source.slot.multivalued), bool(source.slot.required)),
                target=End(tgt.cls, tgt.slot.name, bool(tgt.slot.multivalued), bool(tgt.slot.required)),
                cardinality=cardinality,
                on_delete=rule if rule in ON_DELETE_RULES else "restrict",
                stored=stored,
            )
        )
    return relationships, problems
