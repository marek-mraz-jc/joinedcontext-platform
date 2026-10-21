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
from rdflib import BNode, Graph, Literal, URIRef
from rdflib.collection import Collection
from rdflib.namespace import OWL, RDF, SH, XSD

from common import (
    JSONLD_KEYWORD_ANNOTATION,
    ModelError,
    annotation_value,
    generator_version,
    load,
    ngsi_ld_kind,
    uri_of,
)
from gen_context import compile_context
from relationships import analyse

OPEN_WORLD_ANNOTATION = "open_world"


def _open_world_classes(view: SchemaView) -> list[str]:
    """The classes that accept undeclared attributes (DM-28)."""
    return [
        uri_of(view, cls)
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
        path = URIRef(uri_of(view, slot))
        if annotation_value(slot, JSONLD_KEYWORD_ANNOTATION):
            # `id` and `type` are JSON-LD keywords: in the expanded graph the first is the
            # node's own IRI and the second is `rdf:type`, which the shape already ignores.
            # A property shape for either fails on every valid entity.
            for shape in list(graph.subjects(SH.path, path)):
                graph.remove((shape, None, None))
                graph.remove((None, SH.property, shape))
            continue
        kind = ngsi_ld_kind(slot)
        target = view.all_classes().get(slot.range or "")
        if kind == "Property" and target is not None and "Entity" not in view.class_ancestors(target.name):
            # An inline object expands to a blank node with no `rdf:type`: it is held to the
            # shape of its class, never asked to be an instance of it (T-5066).
            node = URIRef(uri_of(view, target))
            for shape in graph.subjects(SH.path, path):
                if (shape, SH["class"], node) in graph:
                    graph.remove((shape, SH["class"], node))
                    graph.set((shape, SH.node, node))
            continue
        if kind in ("JsonProperty", "GeoProperty", "ListProperty"):
            _apply_structured_kind(graph, path, kind)
            continue
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


def _apply_structured_kind(graph: Graph, path: URIRef, kind: str) -> None:
    """The three kinds whose value the `@context` does not expand as the LinkML range says (T-5066).

    A JsonProperty is `@type: @json`, one `rdf:JSON` literal however deep the object; a
    GeoProperty is a GeoJSON geometry, a node; a ListProperty is `@container: @list`, one RDF
    list whose members carry the range. The shapes LinkML rendered asked for a literal of the
    range or an instance of the inline class, so every valid entity carrying one failed.
    """
    for shape in list(graph.subjects(SH.path, path)):
        datatype = graph.value(shape, SH.datatype)
        for constraint in (SH.datatype, SH["class"], SH.node, SH.pattern, SH["in"]):
            graph.remove((shape, constraint, None))
        if kind == "JsonProperty":
            graph.set((shape, SH.datatype, RDF.JSON))
            graph.set((shape, SH.nodeKind, SH.Literal))
            continue
        graph.set((shape, SH.nodeKind, SH.BlankNodeOrIRI))
        if kind == "GeoProperty" or datatype is None:
            continue
        # The members keep the range: a second shape on the same node walks the list.
        rest, members = BNode(), BNode()
        sequence = BNode()
        Collection(graph, sequence, [path, rest, RDF.first])
        graph.add((rest, SH.zeroOrMorePath, RDF.rest))
        graph.add((members, SH.path, sequence))
        graph.add((members, SH.datatype, datatype))
        for owner in list(graph.subjects(SH.property, shape)):
            graph.add((owner, SH.property, members))


def _agree_with_context(source: str | Path, graph: Graph) -> None:
    """A term the `@context` types as an XSD literal is that literal in the shapes too (T-5066).

    LinkML renders a `uri` range as `sh:nodeKind sh:IRI`, while the context it generates types
    the same term `xsd:anyURI`, which expands to a literal. A consumer expanding a response with
    the served context then saw every link fail its shape.
    """
    context = compile_context(source)["@context"]

    def expand(curie: str) -> str:
        prefix, _, local = curie.partition(":")
        base = context.get(prefix, str(XSD) if prefix == "xsd" else None)
        return base + local if isinstance(base, str) and not local.startswith("//") else curie

    for definition in context.values():
        if not isinstance(definition, dict):
            continue
        datatype, term = definition.get("@type"), definition.get("@id")
        if not (isinstance(datatype, str) and datatype.startswith("xsd:") and isinstance(term, str)):
            continue
        for shape in list(graph.subjects(SH.path, URIRef(expand(term)))):
            if (shape, SH.nodeKind, SH.IRI) in graph:
                graph.set((shape, SH.nodeKind, SH.Literal))
                graph.set((shape, SH.datatype, URIRef(expand(datatype))))


def _drop_computed_ends(view: SchemaView, graph: Graph) -> None:
    """The computed end of a relationship is a query, never an attribute (DM-67).

    Its property shape goes, so the closed shape of its class refuses an entity carrying it; the
    stored end keeps the `sh:class`, `sh:nodeKind sh:IRI` and counts LinkML rendered (DM-72).
    """
    relationships, _ = analyse(view)
    for rel in relationships:
        computed = rel.computed_end
        node = URIRef(uri_of(view, view.get_class(computed.cls)))
        path = URIRef(uri_of(view, view.induced_slot(computed.slot, computed.cls)))
        for shape in list(graph.objects(node, SH.property)):
            if (shape, SH.path, path) in graph:
                graph.remove((node, SH.property, shape))
                graph.remove((shape, None, None))


def compile_shacl(source: str | Path) -> str:
    """Render one LinkML document as closed SHACL shapes, in Turtle."""
    view = load(source)
    graph = Graph()
    graph.parse(data=ShaclGenerator(view.schema, closed=True).serialize(), format="turtle")
    _apply_ngsi_ld_kinds(view, graph)
    _agree_with_context(source, graph)
    _drop_computed_ends(view, graph)

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
    text = generator.serialize()
    # LinkML writes `owl:inverseOf` for the pair; a single end is also functional (DM-72).
    single = [
        end
        for rel in analyse(view)[0]
        for end in (rel.source, rel.target)
        if not end.multivalued
    ]
    if not single:
        return text
    graph = Graph().parse(data=text, format="turtle")
    for end in single:
        graph.add((URIRef(uri_of(view, view.induced_slot(end.slot, end.cls))), RDF.type, OWL.FunctionalProperty))
    return graph.serialize(format="turtle")


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
