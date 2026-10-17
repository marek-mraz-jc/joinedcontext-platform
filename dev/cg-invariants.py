#!/usr/bin/env python3
"""Security invariants as code-graph queries (T-3682, the Antares checker of T-3587).

Runs every statement of dev/security.cypher through `cg q` on the clone's graph. Each statement
returns the VIOLATIONS of one invariant, named by the `// name: ...` comment above it. A row is a
violation unless ci/cg-allowlist.txt lists it, as `name | key | reason`, where the key is the row
for a Cypher statement and the enclosing symbol (else the file) for a GREP hit. GREP hits in tests
or comments never count. Red when:
  - a row is not allowlisted;
  - an allowlist entry has no reason, or matches no row (stale: the code moved, so the entry goes);
  - cg says a statement is wrong (`hint:`), or truncated its output.

  dev/cg-invariants.py                 query the graph (build it first with a full `cg build --fg`: a
                                       --reuse graph names other enclosing symbols for GREP hits, and a
                                       --deps graph is refused)
  dev/cg-invariants.py --from FILE     check saved `cg q` output instead (the self-test does)
  dev/cg-invariants.py --selftest

dev/cg-invariants/fixture.patch violates each invariant once: apply it, `cg build --fg`, run; every
statement must show its cg_fixture row, then revert the patch (`git apply -R`).
"""

from __future__ import annotations

import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
QUERIES = ROOT / "dev" / "security.cypher"
ALLOWLIST = ROOT / "ci" / "cg-allowlist.txt"


def sections(output: str) -> dict[str, list[str]]:
    """`{name: [row key, ...]}` from `cg q` output; problems as rows of the name `!`."""
    found: dict[str, list[str]] = {}
    name, columns, rows = "", None, None
    for line in output.splitlines():
        head = re.match(r"### Q\d+: ([a-z0-9-]+):", line)
        if head:
            name, columns, rows = head.group(1), None, []
            found[name] = rows
            continue
        if rows is None:
            continue
        # cg's own words, never a substring a row may hold (`Extracted::truncated`, T-4021)
        if line.startswith(("hint:", "... truncated at ")) or "output stops at" in line:
            found.setdefault("!", []).append(f"{name}: {line}")
        elif not line.strip() or re.match(r"\d+ (rows|hits)", line):
            continue
        elif columns is None:
            columns = [c.strip() for c in line.split(" | ")]
        else:
            cells = [c.strip() for c in line.split(" | ")]
            row = dict(zip(columns, cells))
            if "test" in row:  # a GREP hit
                if row.get("test") == "True" or row.get("comment") == "True":
                    continue
                rows.append(row.get("in") or row["at"].split(":")[0])
            else:
                rows.append(" | ".join(cells))
    return found


def allowlist(text: str) -> tuple[dict[tuple[str, str], str], list[str]]:
    entries, problems = {}, []
    for number, line in enumerate(text.splitlines(), 1):
        if not line.strip() or line.startswith("#"):
            continue
        parts = [p.strip() for p in line.split(" | ")]
        if len(parts) < 3 or not parts[-1]:
            problems.append(f"{ALLOWLIST.name}:{number}: no reason: {line}")
            continue
        entries[(parts[0], " | ".join(parts[1:-1]))] = parts[-1]
    return entries, problems


def check(output: str, allowed_text: str) -> list[str]:
    # A --deps graph indexes the crates' own code, so a dependency's symbols stop being `external`
    # and its calls become rows: the invariants are written for the plain graph (T-3591).
    if "/cg/deps/" in output.split("\n", 1)[0]:
        return ["graph built with --deps: rebuild it with a plain `cg build --fg`"]
    found = sections(output)
    entries, problems = allowlist(allowed_text)
    problems += found.pop("!", [])
    seen = set()
    for name, rows in found.items():
        for key in dict.fromkeys(rows):
            if (name, key) in entries:
                seen.add((name, key))
            else:
                problems.append(f"{name}: {key}")
    problems += [f"stale allowlist entry: {n} | {k}" for (n, k) in entries if (n, k) not in seen]
    return problems


def selftest() -> None:
    output = """graph: built
### Q1: routes-without-decision: a route
2 rows, 1 ms
c.key | h.path
GET /q/health | antares_api::health
POST /x | antares_api::x

### Q2: sql-from-text: sql
3 hits in 2 files
at | in | test | comment | text
a.rs:1 | m::f | False | False | format!("SELECT")
b.rs:2 | m::t | True | False | format!("SELECT")
c.rs:3 |  | False | False | format!("SELECT")
"""
    ok = "routes-without-decision | GET /q/health | antares_api::health | public probe\n"
    ok += "routes-without-decision | POST /x | antares_api::x | reason\n"
    ok += "sql-from-text | m::f | identifiers only\nsql-from-text | c.rs | identifiers only\n"
    assert check(output, ok) == [], check(output, ok)
    # a new row, an entry without a reason, a stale entry, a wrong query
    assert check(output, ok.replace("routes-without-decision | POST /x | antares_api::x | reason\n", "")) == [
        "routes-without-decision: POST /x | antares_api::x"
    ]
    assert any("no reason" in p for p in check(output, ok.replace(" | identifiers only\nsql", " | \nsql")))
    assert check(output, ok + "sql-from-text | gone::f | was here\n") == [
        "stale allowlist entry: sql-from-text | gone::f"
    ]
    assert check(output + "hint: 'x' matches no Symbol\n", ok) == ["sql-from-text: hint: 'x' matches no Symbol"]
    # a row naming `truncated` is a row; cg's own line is a problem (T-4021)
    row = "d.rs:4 | m::Extracted::truncated | False | False | truncated = true\n"
    assert check(output + row, ok + "sql-from-text | m::Extracted::truncated | flag\n") == []
    cut = "... truncated at 60 rows; add LIMIT / aggregate, or --max-rows\n"
    assert check(output + cut, ok) == [f"sql-from-text: {cut.strip()}"]
    deps = "graph: built 2026 [rust:., rust:/home/dev/.cache/cg/deps/axum-0.8.9/src]\n"
    assert check(deps + output.split("\n", 1)[1], ok) == [
        "graph built with --deps: rebuild it with a plain `cg build --fg`"
    ]
    print("cg-invariants selftest: ok")


def main(argv: list[str]) -> int:
    if argv[1:] == ["--selftest"]:
        selftest()
        return 0
    if argv[1:2] == ["--from"]:
        output = pathlib.Path(argv[2]).read_text()
    else:
        run = subprocess.run(
            ["cg", "q", "--max-rows", "10000", str(QUERIES)],
            cwd=ROOT, capture_output=True, text=True, check=False,
        )
        if run.returncode != 0:
            print(run.stdout + run.stderr, file=sys.stderr)
            return 2
        output = run.stdout
    problems = check(output, ALLOWLIST.read_text())
    for p in problems:
        print(p)
    print(f"cg-invariants: {len(problems)} problem(s)" if problems else "cg-invariants: OK")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
