#!/usr/bin/env python3
"""The ngsi-ld: ODRL profile's SHACL shapes refuse what `jcctl policy import` refuses (T-5039).

`cargo test -p jc-core --test policy_odrl_tests` writes every rendered Policy and the broker's
access document into RECORDS_DIR/accepted, and each refused document the shapes can see into
RECORDS_DIR/refused. This reads them with the two contexts of the profile (W3C's ODRL context
pinned beside the test, its digest checked first, so nothing is fetched) and asserts the shapes
in schemas/odrl/ngsi-ld-v1 accept every accepted document and refuse every refused one.

    validate-odrl-profile.py RECORDS_DIR
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import sys

from pyshacl import validate
from rdflib import Graph

ROOT = pathlib.Path(__file__).resolve().parents[2]
PROFILE = ROOT / "schemas/odrl/ngsi-ld-v1"
ODRL = ROOT / "crates/jc-core/tests/fixtures/odrl/odrl.jsonld"
# https://www.w3.org/ns/odrl.jsonld as served 2026-10-23
ODRL_SHA256 = "bb55ab32bc6041c6ef723cbce89510128a4dfb9bec6024777240791b112c8b6c"
CONTEXTS = {
    "http://www.w3.org/ns/odrl.jsonld": ODRL,
    "https://joinedcontext.com/odrl/ngsi-ld/v1/context.jsonld": PROFILE / "context.jsonld",
}


def graph(path: pathlib.Path) -> Graph:
    """The document as RDF, each context URL replaced by its local file."""
    doc = json.loads(path.read_text())
    urls = doc.get("@context")
    if isinstance(urls, list) and all(url in CONTEXTS for url in urls):
        doc["@context"] = [json.loads(CONTEXTS[url].read_text())["@context"] for url in urls]
    return Graph().parse(data=json.dumps(doc), format="json-ld", base="https://records.invalid/")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("records", type=pathlib.Path)
    args = parser.parse_args()
    digest = hashlib.sha256(ODRL.read_bytes()).hexdigest()
    if digest != ODRL_SHA256:
        print(f"{ODRL} is not the pinned W3C context (sha256 {digest})", file=sys.stderr)
        return 1
    shapes = Graph().parse(PROFILE / "shapes.ttl", format="turtle")
    failed = 0
    for expected, sub in ((True, "accepted"), (False, "refused")):
        files = sorted((args.records / sub).glob("*.jsonld"))
        if not files:
            print(f"no records in {args.records / sub}", file=sys.stderr)
            return 1
        for path in files:
            conforms, _, report = validate(graph(path), shacl_graph=shapes)
            if conforms != expected:
                failed += 1
                verdict = "refused" if expected else "accepted"
                print(f"{sub}/{path.name}: the shapes {verdict} it\n{report}", file=sys.stderr)
    print(f"odrl profile: {failed} disagreement(s)")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
