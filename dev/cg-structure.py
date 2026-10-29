#!/usr/bin/env python3
"""Structure ratchets from the code graph (T-4587): complexity, untested public functions and
crate layering may only get better.

Per crate, two numbers, outside tests: functions with cyclomatic complexity >= 25, and public
functions of 20+ lines that no test reaches within three calls (CALLS or DISPATCHES). Beside
them, the workspace's non-dev crate edges (`a -> b` in Cargo.toml). ci/cg-structure-baseline.txt
holds both: a crate above its line is red (a new complex or untested function), below it red
until the line is lowered; an edge not listed is red (a new dependency between our crates), a
listed edge that is gone is red until its line is deleted. The baseline only goes down.

  dev/cg-structure.py              query the graph (`cg build --fg` first) and check
  dev/cg-structure.py --rows       also print every complex and untested function
  dev/cg-structure.py --selftest

cg is not on the CI runner, so tasks/push runs this beside dev/cg-dead-code.py.
"""

from __future__ import annotations

import collections
import importlib.util
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
BASELINE = ROOT / "ci" / "cg-structure-baseline.txt"
_spec = importlib.util.spec_from_file_location("cg_dead_code", ROOT / "dev" / "cg-dead-code.py")
_dead = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_dead)
table, query = _dead.table, _dead.query

CC, LOC, HOPS = 25, 20, 3
FUNCTION = (
    "f.lang='rust' AND NOT f.is_test AND NOT f.external AND NOT f.file STARTS WITH 'vendor/' "
    "AND f.kind IN ['Function','Method','StaticMethod','TraitMethod']"
)
COLUMNS = "f.package, f.path, f.file, f.line, f.cc, f.loc"
COMPLEX = f"MATCH (f:Symbol) WHERE {FUNCTION} AND f.cc >= {CC} RETURN {COLUMNS};"
UNTESTED = (
    f"MATCH (f:Symbol) WHERE {FUNCTION} AND f.visibility='pub' AND f.loc >= {LOC} "
    f"AND NOT EXISTS {{ MATCH (t:Symbol)-[:CALLS|DISPATCHES*1..{HOPS}]->(f) WHERE t.is_test }} "
    f"RETURN {COLUMNS};"
)
EDGES = (
    "MATCH (a:Package)-[r:DEPENDS_ON]->(b:Package) WHERE a.workspace AND b.workspace "
    "AND r.kind <> 'dev' AND a.name <> b.name RETURN a.name, b.name;"
)


def baseline(text: str) -> tuple[dict[str, tuple[int, int]], set[str]]:
    """`crate | complex | untested` lines and `edge | a -> b` lines."""
    counts, edges = {}, set()
    for line in text.splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        parts = [p.strip() for p in line.split("|")]
        if parts[0] == "edge":
            edges.add(parts[1])
        else:
            counts[parts[0]] = (int(parts[1]), int(parts[2]))
    return counts, edges


def check(complex_rows: list[dict[str, str]], untested_rows: list[dict[str, str]],
          edges: set[str], allowed: dict[str, tuple[int, int]], allowed_edges: set[str]
          ) -> list[str]:
    problems = []
    for what, rows, at in (("complex", complex_rows, 0), ("untested", untested_rows, 1)):
        have = collections.Counter(r["f.package"] for r in rows)
        for name in sorted(set(have) | set(allowed)):
            n, may = have.get(name, 0), allowed.get(name, (0, 0))[at]
            if n > may:
                problems.append(f"{name}: {n} {what}, baseline {may}; the functions:")
                problems += [f"  {r['f.file']}:{r['f.line']} {r['f.path']} cc {r['f.cc']} "
                             f"loc {r['f.loc']}" for r in rows if r["f.package"] == name][:40]
            elif n < may:
                problems.append(f"{name}: {n} {what}, baseline {may}: lower its line in "
                                f"{BASELINE.relative_to(ROOT)}")
    problems += [f"new crate edge {e}: depend on a library crate instead, or list it with a "
                 f"reason" for e in sorted(edges - allowed_edges)]
    problems += [f"crate edge {e} is gone: delete its line in {BASELINE.relative_to(ROOT)}"
                 for e in sorted(allowed_edges - edges)]
    return problems


def selftest() -> None:
    def row(pkg, path, cc="30", loc="40"):
        return {"f.package": pkg, "f.path": path, "f.file": "crates/x/src/lib.rs",
                "f.line": "1", "f.cc": cc, "f.loc": loc}
    complex_rows = [row("jcctl", "jcctl::main"), row("jc-core", "jc_core::validate")]
    untested = [row("jcctl", "jcctl::run", cc="3")]
    edges = {"jcctl -> jc-core", "context-gateway -> jcctl"}
    allowed = {"jcctl": (1, 1), "jc-core": (1, 0)}
    assert check(complex_rows, untested, edges, allowed, set(edges)) == []
    # a new complex function in a crate is red and names it
    grown = check(complex_rows + [row("jcctl", "jcctl::sprawl")], untested, edges, allowed,
                  set(edges))
    assert grown[0] == "jcctl: 2 complex, baseline 1; the functions:", grown
    assert any("jcctl::sprawl" in p for p in grown), grown
    # a crate the baseline does not list may not gain one either
    assert check(complex_rows + [row("assistant", "a::b")], untested, edges, allowed,
                 set(edges))[0].startswith("assistant: 1 complex, baseline 0")
    # an improvement is red until the line is lowered, and green once it is
    better = check(complex_rows[:1], untested, edges, allowed, set(edges))
    assert better == ["jc-core: 0 complex, baseline 1: lower its line in "
                      "ci/cg-structure-baseline.txt"], better
    assert check(complex_rows[:1], untested, edges, allowed | {"jc-core": (0, 0)},
                 set(edges)) == []
    # untested counts on their own column
    assert check(complex_rows, untested + [row("jc-core", "jc_core::load", cc="2")], edges,
                 allowed, set(edges))[0].startswith("jc-core: 1 untested, baseline 0")
    # a new edge between our crates is red, a removed one red until its line goes
    assert check(complex_rows, untested, edges | {"functions -> context-gateway"}, allowed,
                 set(edges))[0].startswith("new crate edge functions -> context-gateway")
    assert check(complex_rows, untested, {"jcctl -> jc-core"}, allowed, set(edges)) == [
        "crate edge context-gateway -> jcctl is gone: delete its line in "
        "ci/cg-structure-baseline.txt"]
    counts, listed = baseline("# c\njcctl | 14 | 15\n\nedge | jcctl -> jc-core\n")
    assert counts == {"jcctl": (14, 15)} and listed == {"jcctl -> jc-core"}
    print("cg-structure selftest: ok")


def main(argv: list[str]) -> int:
    if argv[1:] == ["--selftest"]:
        selftest()
        return 0
    complex_rows, untested = table(query(COMPLEX)), table(query(UNTESTED))
    edges = {f"{r['a.name']} -> {r['b.name']}" for r in table(query(EDGES))}
    complexes = collections.Counter(r["f.package"] for r in complex_rows)
    untests = collections.Counter(r["f.package"] for r in untested)
    for name in sorted(set(complexes) | set(untests)):
        print(f"{name} | {complexes.get(name, 0)} | {untests.get(name, 0)}")
    for e in sorted(edges):
        print(f"edge | {e}")
    if "--rows" in argv:
        for what, rows in (("complex", complex_rows), ("untested", untested)):
            for r in sorted(rows, key=lambda r: -int(r["f.cc"] or 0)):
                print(f"{what} | {r['f.file']}:{r['f.line']} | {r['f.path']} | cc {r['f.cc']} "
                      f"| loc {r['f.loc']}")
    allowed, allowed_edges = baseline(BASELINE.read_text())
    problems = check(complex_rows, untested, edges, allowed, allowed_edges)
    for p in problems:
        print(p)
    print(f"cg-structure: {len(problems)} problem(s)" if problems else "cg-structure: OK")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
