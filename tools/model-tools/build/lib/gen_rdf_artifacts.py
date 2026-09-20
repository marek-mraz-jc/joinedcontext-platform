"""LinkML → SHACL shapes and OWL ontology (T-0170, DM-43, DM-46).

Both come out of one run so they cannot disagree with each other or with the JSON Schema
(DM-43). The shapes are closed: a payload carrying an attribute the model does not declare is
a validation failure, not an extension (DM-28). A class may opt out with `open_world: true`,
and only that class opens.

The SHACL artifact is meant to be usable by a consumer's own validator against the
`ngsi-ld/v1` responses expanded with the served `@context` (DM-46), so it is written in
Turtle with the model's own prefixes.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from linkml.generators.owlgen import OwlSchemaGenerator
from linkml.generators.shaclgen import ShaclGenerator
from linkml_runtime import SchemaView
from rdflib import Graph, Literal, URIRef
from rdflib.namespace import SH, XSD

from common import (
    JSONLD_KEYWORD_ANNOTATION,
    ModelError,
    annotation_value,
    generator_version,
    load,
    ngsi_ld_kind,
)

OPEN_WORLD_ANNOTATION = "open_world"


def _open_world_classes(view: SchemaView) -> list[str]:
    """The classes that accept undeclared attributes (DM-28)."""
    return [
        view.get_uri(cls, expand=True)
        for cls in view.all_classes().values()
        if (annotation_value(cls, OPEN_WORLD_ANNOTATION) or "").lower() == "true"
    ]


def _apply_ngsi_ld_kinds(view: SchemaView, graph: Graph) -> None:
    """Make the shapes agree with the `@context` about what a value is (DM-43, DM-46).

    LinkML derives the constraint from the LinkML range, which for both a Relationship and a
    LanguageProperty is a string. The `@context` expands the first into an IRI and the second
    into language-tagged literals, so a consumer validating a real `ngsi-ld/v1` response
    against the untouched shapes would see every relationship and every translated label fail.
    """
    for slot in view.all_slots().values():
        path = URIRef(view.get_uri(slot, expand=True))
        if annotation_value(slot, JSONLD_KEYWORD_ANNOTATION):
            # `id` and `type` are JSON-LD keywords: in the expanded graph the first is the
            # node's own IRI and the second is `rdf:type`, which the shape already ignores.
            # A property shape for either fails on every valid entity.
            for shape in list(graph.subjects(SH.path, path)):
                graph.remove((shape, None, None))
                graph.remove((None, SH.property, shape))
            continue
        kind = ngsi_ld_kind(slot)
        if kind not in ("Relationship", "LanguageProperty"):
            continue
        for shape in graph.subjects(SH.path, path):
            graph.remove((shape, SH.datatype, None))
            if kind == "Relationship":
                # The object is the id of another entity, so it is an IRI, never a literal.
                graph.set((shape, SH.nodeKind, SH.IRI))
            else:
                # A language map expands to one literal per locale, which is several values
                # for one path; the cardinality the range implied does not survive that.
                graph.remove((shape, SH.maxCount, None))
                graph.set((shape, SH.nodeKind, SH.Literal))
                graph.set((shape, SH.uniqueLang, Literal(True, datatype=XSD.boolean)))


def compile_shacl(source: str | Path) -> str:
    """Render one LinkML document as closed SHACL shapes, in Turtle."""
    view = load(source)
    graph = Graph()
    graph.parse(data=ShaclGenerator(view.schema, closed=True).serialize(), format="turtle")
    _apply_ngsi_ld_kinds(view, graph)

    # The generator's `closed` flag is schema-wide, so the per-class opt-out is applied to the
    # rendered graph: the shape of an open class stops being closed, every other shape stays.
    for class_uri in _open_world_classes(view):
        shape = URIRef(class_uri)
        graph.set((shape, SH.closed, Literal(False, datatype=XSD.boolean)))

    graph.add(
        (
            URIRef(view.schema.id),
            URIRef("https://w3id.org/linkml/generator_version"),
            Literal(generator_version()),
        )
    )
    return graph.serialize(format="turtle")


def compile_owl(source: str | Path) -> str:
    """Render one LinkML document as an OWL ontology, in Turtle."""
    view = load(source)
    # The three flags are LinkML's future defaults. Pinning them here keeps the ontology
    # stable across generator upgrades, which is what DM-44's byte-identical rebuild needs.
    generator = OwlSchemaGenerator(
        view.schema,
        format="ttl",
        skip_vacuous_min_zero_cardinality_axioms=True,
        skip_vacuous_local_range_axioms=True,
        consolidate_cardinality_axioms=True,
    )
    return generator.serialize()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("source", help="LinkML YAML file")
    parser.add_argument(
        "--artifact", choices=("shacl", "owl"), default="shacl", help="which one to render"
    )
    parser.add_argument("-o", "--output", help="write here instead of stdout")
    args = parser.parse_args(argv)

    try:
        text = compile_shacl(args.source) if args.artifact == "shacl" else compile_owl(args.source)
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
