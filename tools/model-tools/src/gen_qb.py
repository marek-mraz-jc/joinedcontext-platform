"""LinkML → RDF Data Cube structure, `model.qb.ttl` (T-1188, DM-60, Architecture/03 §2).

A model that declares a Data Structure Definition (a class annotated `qb_dsd: true`) renders one
`qb:DataStructureDefinition` per such class: its components in slot order, each dimension typed
`qb:DimensionProperty` and each measure `qb:MeasureProperty`, and the class itself a subclass of
`qb:Observation`, which is what its entities are. A model without one renders nothing: the
artifact is absent, never an empty cube.

The Turtle is written, not serialised through rdflib: its blank nodes would be named anew on
every run, and the artifact has to be byte-identical across rebuilds like the rest of the set
(DM-44).
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from linkml_runtime import SchemaView
from linkml_runtime.linkml_model.meta import ClassDefinition, SlotDefinition

from common import ModelError, dsd_components, is_dsd, load, ngsi_ld_kind

QB = "http://purl.org/linked-data/cube#"

#: The XSD datatype of a LinkML range, where the cube can state one.
XSD = {
    "integer": "xsd:integer",
    "float": "xsd:double",
    "double": "xsd:double",
    "decimal": "xsd:decimal",
    "boolean": "xsd:boolean",
    "date": "xsd:date",
    "datetime": "xsd:dateTime",
    "string": "xsd:string",
}


def _escape(text: str) -> str:
    return (
        text.replace("\\", "\\\\")
        .replace('"', '\\"')
        .replace("\n", "\\n")
        .replace("\r", "\\r")
        .replace("\t", "\\t")
    )


def _comment(element: ClassDefinition | SlotDefinition) -> str:
    text = element.description
    return f' ;\n  rdfs:comment "{_escape(str(text).strip())}"' if text else ""


def _range(view: SchemaView, slot: SlotDefinition) -> str:
    """`rdfs:range`, where the slot's range is a datatype or a class of the model."""
    target = slot.range or "string"
    if ngsi_ld_kind(slot) == "Relationship":
        # An external reference names no class of ours; a class range is that class.
        return f" ;\n  rdfs:range <{view.get_uri(target, expand=True)}>" if target in view.all_classes() else ""
    datatype = XSD.get(target)
    return f" ;\n  rdfs:range {datatype}" if datatype else ""


def _structure(view: SchemaView, cls: ClassDefinition) -> str:
    class_iri = view.get_uri(cls, expand=True)
    components = dsd_components(view, cls)
    lines = [
        f"<{class_iri}Structure> a qb:DataStructureDefinition ;",
        f'  rdfs:label "{_escape(cls.name)}"{_comment(cls)} ;',
        "  qb:component",
    ]
    entries = []
    for order, (slot, role) in enumerate(components, start=1):
        slot_iri = view.get_uri(slot, expand=True)
        entries.append(f"    [ qb:{role} <{slot_iri}> ; qb:order {order} ]")
    lines.append(" ,\n".join(entries) + " .")
    out = "\n".join(lines) + "\n"
    out += f"\n<{class_iri}> rdfs:subClassOf qb:Observation .\n"
    for slot, role in components:
        kind = "qb:DimensionProperty" if role == "dimension" else "qb:MeasureProperty"
        out += (
            f"\n<{view.get_uri(slot, expand=True)}> a rdf:Property, {kind} ;\n"
            f'  rdfs:label "{_escape(slot.name)}"{_range(view, slot)}{_comment(slot)} .\n'
        )
    return out


def render_qb(view: SchemaView) -> str | None:
    """The cube structure of every DSD class of a loaded model, or None when it declares none."""
    classes = sorted((cls for cls in view.all_classes().values() if is_dsd(cls)), key=lambda c: c.name)
    if not classes:
        return None
    out = (
        # No generator version here: the run reports it once for the whole set (DM-43), and a
        # golden file that changed with every LinkML release would say nothing.
        f"# RDF Data Cube structure of the model {view.schema.name} (DM-60).\n"
        f"@prefix qb: <{QB}> .\n"
        "@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .\n"
        "@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n"
        "@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n"
    )
    for cls in classes:
        out += "\n" + _structure(view, cls)
    return out


def compile_qb(source: str | Path) -> str | None:
    """Render one LinkML document's Data Structure Definitions as RDF Data Cube Turtle."""
    return render_qb(load(source))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("source", help="LinkML YAML file")
    parser.add_argument("-o", "--output", help="write here instead of stdout")
    args = parser.parse_args(argv)
    try:
        text = compile_qb(args.source)
    except ModelError as err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    if text is None:
        print("error: the model declares no Data Structure Definition (qb_dsd: true)", file=sys.stderr)
        return 1
    if args.output:
        Path(args.output).write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
