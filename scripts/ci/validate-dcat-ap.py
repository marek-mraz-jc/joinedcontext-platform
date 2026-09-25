#!/usr/bin/env python3
"""Every Endpoint record variant passes the SEMIC DCAT-AP 3.0 shapes (T-2789, EP-78).

`cargo test -p context-gateway --test dcat_ap_catalog_tests` writes each record variant as
JSON-LD and Turtle into a directory; this reads them back, validates each serialisation against
the SEMIC shapes pinned beside the test, and checks that the two serialisations of one variant
are the same graph, because the Turtle is written from the JSON-LD and must say nothing else.

The shapes are the upstream file, byte for byte; its digest is checked first, so a local edit
that relaxes a shape fails here instead of passing everything after it.

    validate-dcat-ap.py RECORDS_DIR [--shapes PATH]
"""

from __future__ import annotations

import argparse
import hashlib
import pathlib
import sys

from pyshacl import validate
from rdflib import Graph
from rdflib.compare import isomorphic

ROOT = pathlib.Path(__file__).resolve().parents[2]
SHAPES = ROOT / "crates/context-gateway/tests/fixtures/dcat-ap/dcat-ap-3.0.0-SHACL.ttl"
# SEMICeu/DCAT-AP releases/3.0.0/shacl/dcat-ap-SHACL.ttl
SHAPES_SHA256 = "92f76609d78d257123e75bc6b7155df5cc0a63f14c29fbc12b0ac95c56af2059"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("records", type=pathlib.Path)
    parser.add_argument("--shapes", type=pathlib.Path, default=SHAPES)
    args = parser.parse_args()

    digest = hashlib.sha256(args.shapes.read_bytes()).hexdigest()
    if args.shapes == SHAPES and digest != SHAPES_SHA256:
        print(f"{args.shapes} is not the pinned SEMIC file (sha256 {digest})", file=sys.stderr)
        return 1
    shapes = Graph().parse(args.shapes, format="turtle")

    variants = sorted(args.records.glob("*.jsonld"))
    if not variants:
        print(f"no record variants in {args.records}: run the dcat_ap_catalog_tests first",
              file=sys.stderr)
        return 1

    failures = 0
    for jsonld in variants:
        turtle = jsonld.with_suffix(".ttl")
        graphs = {
            "JSON-LD": Graph().parse(jsonld, format="json-ld"),
            "Turtle": Graph().parse(turtle, format="turtle"),
        }
        for name, graph in graphs.items():
            conforms, _, report = validate(graph, shacl_graph=shapes, advanced=True)
            if not conforms:
                failures += 1
                print(f"{jsonld.stem} ({name}) does not conform:\n{report}", file=sys.stderr)
        if not isomorphic(graphs["JSON-LD"], graphs["Turtle"]):
            failures += 1
            only_json = graphs["JSON-LD"] - graphs["Turtle"]
            only_turtle = graphs["Turtle"] - graphs["JSON-LD"]
            print(f"{jsonld.stem}: the two serialisations are different graphs\n"
                  f"only in JSON-LD:\n{only_json.serialize(format='nt')}\n"
                  f"only in Turtle:\n{only_turtle.serialize(format='nt')}", file=sys.stderr)
        if not failures:
            print(f"{jsonld.stem}: conforms, one graph ({len(graphs['Turtle'])} triples)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
