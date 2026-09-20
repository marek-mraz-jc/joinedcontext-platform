"""LinkML → JSON-LD `@context` with NGSI-LD kind bindings (T-0169, DM-02, DM-04, DM-05, DM-18).

`gen-jsonld-context` writes every term as a bare name resolved by `@vocab`, and it knows
nothing about NGSI-LD. Two things are added here:

* the IRI of every class and slot is bound explicitly, so a consumer that resolves a term
  gets the organisation's IRI and not whatever `@vocab` happens to be at expansion time;
* the `ngsi_ld_kind` of a slot becomes the JSON-LD keyword that makes NGSI-LD payloads expand
  correctly: `@type: @id` for a Relationship, `@container: @language` for a LanguageProperty
  (DM-05).
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

from linkml.generators.jsonldcontextgen import ContextGenerator
from linkml_runtime import SchemaView

from common import (
    JSONLD_KEYWORD_ANNOTATION,
    UPSTREAM_ANNOTATION,
    ModelError,
    annotation_value,
    generator_version,
    load,
    ngsi_ld_kind,
    reserved_namespace,
)


def _term(view: SchemaView, element: Any) -> str:
    """The IRI of a class or slot, as a CURIE where a prefix covers it."""
    expanded = view.get_uri(element, expand=True)
    return view.namespaces().curie_for(expanded, default_ok=False) or expanded


def check_namespaces(view: SchemaView) -> None:
    """Refuse a model that mints its own terms under someone else's namespace (DM-04, DM-16).

    Reusing an upstream IRI is how a model says "this is the same term"; the importer marks
    those slots with their source. Minting a *new* term under the Smart Data Models, ETSI or
    W3C namespace claims authority the organisation does not have, and every consumer that
    dereferences the IRI gets a page that does not describe this model.
    """
    squatted = []
    for element in list(view.all_classes().values()) + list(view.all_slots().values()):
        iri = view.get_uri(element, expand=True)
        namespace = reserved_namespace(iri)
        if namespace and not annotation_value(element, UPSTREAM_ANNOTATION):
            squatted.append(f"'{element.name}' → {iri} (reserved: {namespace})")
    if squatted:
        raise ModelError(
            "these terms are minted under a namespace the organisation does not own; "
            "use the organisation's own prefix, or import the term and keep its "
            f"{UPSTREAM_ANNOTATION}: " + "; ".join(sorted(squatted))
        )


def compile_context(source: str | Path) -> dict[str, Any]:
    """Render one LinkML document as a JSON-LD `@context`."""
    view = load(source)
    check_namespaces(view)

    document = json.loads(ContextGenerator(view.schema).serialize())
    context: dict[str, Any] = document.setdefault("@context", {})

    for cls in view.all_classes().values():
        # A class is a type IRI, never a term with a container or a value type.
        context[cls.name] = {"@id": _term(view, cls)}

    for slot in view.all_slots().values():
        name = slot.alias or slot.name

        keyword = annotation_value(slot, JSONLD_KEYWORD_ANNOTATION)
        if keyword:
            # `id` and `type` are JSON-LD keywords, not terms. The NGSI-LD core context binds
            # them that way and this context is used *after* it, so binding them to an IRI
            # here would override the core and leave every entity without a subject or a
            # type: the entity then matches no shape and validation passes vacuously.
            context[name] = keyword
            continue

        existing = context.get(name)
        entry: dict[str, Any] = dict(existing) if isinstance(existing, dict) else {}
        entry["@id"] = _term(view, slot)
        kind = ngsi_ld_kind(slot)

        if kind == "Relationship":
            # The object of a Relationship is an entity id, so it expands as an IRI and not
            # as a string; without this the value is a literal and no consumer can follow it.
            entry["@type"] = "@id"
        elif kind == "LanguageProperty":
            # A language map: {"sk": "…", "en": "…"}. A value type here would fight the map.
            entry.pop("@type", None)
            entry["@container"] = "@language"
        elif entry.get("@type") == "@id":
            # LinkML writes `@type: @id` for every slot whose range is a class, which is
            # right for a Relationship and wrong for a JsonProperty: an imported `address`
            # carries an inline object, and a context that calls it an IRI makes every
            # consumer resolve the object as one and lose it. The `ngsi_ld_kind` annotation
            # is what decides (DM-05), so an inherited `@id` typing is taken back off.
            entry.pop("@type")

        # The kind itself stays out of the term definition: JSON-LD 1.1 rejects a term
        # definition carrying a key it does not know, and an invalid @context expands to
        # nothing. The kind travels in the JSON Schema (`x-ngsi-ld-kind`) and in the source.
        context[name] = entry

    document["@context"] = context
    document["x-generator-version"] = generator_version()
    return document


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("source", help="LinkML YAML file")
    parser.add_argument("-o", "--output", help="write here instead of stdout")
    args = parser.parse_args(argv)

    try:
        document = compile_context(args.source)
    except ModelError as err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    text = json.dumps(document, indent=2, sort_keys=True) + "\n"
    if args.output:
        Path(args.output).write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
