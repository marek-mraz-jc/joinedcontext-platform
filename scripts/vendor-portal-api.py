#!/usr/bin/env python3
"""Vendors the part of the Portal OpenAPI jcctl calls (T-5966) into crates/jcctl/portal-api.json.

    scripts/vendor-portal-api.py <portal clone> <commit>          # write the extract
    scripts/vendor-portal-api.py <portal clone> <commit> --check  # exit 1 when it differs

It reads ui/openapi.json at <commit> of the Portal clone (git show, so the clone may sit on any
branch) and keeps what jcctl's command table depends on: the resource paths and the operation
registry paths with their methods and query parameters, the kinds jc_schema_get serves and the
verb each registry operation requires. crates/jcctl/tests/portal_api_drift_tests.rs fails when the
command table and this extract disagree.
"""

import hashlib
import json
import subprocess
import sys
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "crates/jcctl/portal-api.json"
PROJECT = "/api/v1/projects/{project}"
KEPT = (f"{PROJECT}/{{plural}}", f"{PROJECT}/{{plural}}/{{name}}", f"{PROJECT}/ops")
OPS = f"{PROJECT}/ops/"


def extract(text: str, commit: str) -> dict:
    doc = json.loads(text)
    paths = {}
    ops = []
    for path, item in doc["paths"].items():
        if path not in KEPT and not path.startswith(OPS):
            continue
        if path == f"{OPS}{{name}}":
            # The generic door every per-operation entry below repeats with its own schema.
            continue
        methods = {
            method.upper(): sorted(p["name"] for p in op.get("parameters", []) if p["in"] == "query")
            for method, op in item.items()
            if method in ("get", "post", "put", "patch", "delete")
        }
        if path.startswith(OPS):
            post = item["post"]
            ops.append(
                {
                    "name": path[len(OPS):],
                    "verb": post.get("x-jc-requires", {}).get("verb"),
                    "description": post.get("description", ""),
                }
            )
        else:
            paths[path] = methods
    schema_get = doc["paths"][f"{OPS}jc_schema_get"]["post"]
    kinds = schema_get["requestBody"]["content"]["application/json"]["schema"]["properties"]["kind"]["enum"]
    return {
        "source": {
            "repository": "marek-mraz/joinedcontext-portal",
            "commit": commit,
            "file": "ui/openapi.json",
            "sha256": hashlib.sha256(text.encode()).hexdigest(),
        },
        "paths": paths,
        "kinds": sorted(kinds),
        "ops": sorted(ops, key=lambda o: o["name"]),
    }


def main(argv: list) -> int:
    if len(argv) not in (2, 3) or (len(argv) == 3 and argv[2] != "--check"):
        print(__doc__, file=sys.stderr)
        return 2
    clone, commit = argv[0], argv[1]
    commit = subprocess.run(
        ["git", "-C", clone, "rev-parse", "--verify", f"{commit}^{{commit}}"],
        check=True, capture_output=True, text=True,
    ).stdout.strip()
    text = subprocess.run(
        ["git", "-C", clone, "show", f"{commit}:ui/openapi.json"],
        check=True, capture_output=True, text=True,
    ).stdout
    wanted = json.dumps(extract(text, commit), indent=2, sort_keys=True) + "\n"
    if len(argv) == 3:
        if OUT.read_text() != wanted:
            print(f"{OUT} is not the extract of {commit}: rerun without --check", file=sys.stderr)
            return 1
        return 0
    OUT.write_text(wanted)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
