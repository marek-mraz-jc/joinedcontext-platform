#!/usr/bin/env python3
"""Code no production entry point reaches, per crate, as a ratchet (T-3918, GOAL point 11).

Entry points: every binary's `main` (`src/main.rs`, `src/bin/*.rs`, `build.rs`), every pub item
of `jc-app-sdk` (the Apps' SDK), and every pub item of `jc-core`, `jcctl` and `jc-manifest` (which
jcctl re-exports) that the Portal's source names (a method only when its type is named too); the
Portal links those crates, and `context-gateway` only in its tests. A symbol is reached through CALLS, DISPATCHES and REFERENCES
from an entry point; a type reached also reaches its trait impls (derives, `From` through `?`,
`Display` in `format!`), which the graph has no call edge for, and its fields and variants, so
the types a reached type holds are reached. What is left, outside tests,
examples and `build.rs` helpers, is a row: a function, method, type, constant or macro.

The rows are judged one by one in docs/analysis/platform-dead-code.md. This check keeps their
number from growing: ci/cg-dead-code-baseline.txt holds `crate | rows`, and a crate with more
rows than its line is red (new code nothing calls, or a caller deleted and its callees kept),
a crate with fewer is red too until the line is lowered, so the baseline only goes down.

  dev/cg-dead-code.py                    query the graph (`cg build --fg` first) and check
  dev/cg-dead-code.py --rows             also print every row, by file
  dev/cg-dead-code.py --from DIR         read DIR/symbols.txt, edges.txt, members.txt (saved `cg q`)
  dev/cg-dead-code.py --portal SRC       the Portal's src/ (default ../joinedcontext-portal/src)
  dev/cg-dead-code.py --selftest

A function a serde attribute names by string (`default = "f"`, `skip_serializing_if = "f"`,
`deserialize_with`, `serialize_with`) anywhere in its crate is reached, and so is a constant a
format string of its crate captures (`"{PAGE_LIMIT}"`): cg has no edge for a string. A
`#[global_allocator]` static is an entry point, as `main` is: the compiler calls it.
The other blind spots, a `macro_rules!` body or a closure stored in a field, stay rows; the doc
checks each row with GREP before a delete.
"""

from __future__ import annotations

import collections
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
BASELINE = ROOT / "ci" / "cg-dead-code-baseline.txt"
SYMBOLS = (
    "MATCH (s:Symbol) WHERE s.lang='rust' AND NOT s.external RETURN s.id, s.path, s.kind, "
    "s.package, s.file, s.line, s.loc, s.visibility, s.is_test, s.trait_name, s.self_type;"
)
EDGES = (
    "MATCH (a:Symbol)-[r:CALLS|DISPATCHES|REFERENCES]->(b:Symbol) WHERE a.lang='rust' "
    "AND NOT b.external RETURN a.id, b.id;"
)
# A type reaches its fields and variants, and through them the types they hold.
MEMBERS = (
    "MATCH (a:Symbol)-[:CONTAINS]->(b:Symbol) WHERE a.lang='rust' AND NOT a.external "
    "AND a.kind IN ['Struct','Enum'] RETURN a.id, b.id;"
)
KINDS = {"Function", "Method", "StaticMethod", "TraitMethod", "Struct", "Enum", "Constant",
         "StaticVariable", "TypeAlias", "Trait", "Macro"}
SDK = "jc-app-sdk"
SERDE_FN = re.compile(
    r'\b(?:default|skip_serializing_if|deserialize_with|serialize_with)\s*=\s*"(?:[\w:]*?(\w+)::)?(\w+)(?:::<[^"]*>)?"')
# A const named only as an inline format capture (`"{PAGE_LIMIT}"`) has no edge in the graph.
CAPTURE = re.compile(r'\{([A-Z][A-Z0-9_]*)[}:]')
# The allocator a binary declares (T-4782): nothing names it, the compiler links it.
ALLOCATOR = re.compile(r'#\[global_allocator\]\s*(?:pub(?:\([^)]*\))?\s+)?static\s+(\w+)')
# jc-manifest too: the Portal names its modules through jcctl's re-exports (T-4604).
PORTAL_LINKS = ("jc-core", "jcctl", "jc-manifest")


def table(text: str) -> list[dict[str, str]]:
    """The rows of one `cg q` table; problems (a hint, a truncation) raise."""
    lines = text.splitlines()
    for line in lines:
        if line.startswith("hint:") or "output stops at" in line:
            raise ValueError(line)
    start = next((i for i, line in enumerate(lines) if line.startswith("### Q")), -1)
    head = next(i for i, line in enumerate(lines) if i > start and " | " in line)
    cols = [c.strip() for c in lines[head].split(" | ")]
    return [dict(zip(cols, (c.strip() for c in line.split(" | "))))
            for line in lines[head + 1:] if line.count(" | ") == len(cols) - 1]


def production(s: dict[str, str]) -> bool:
    f = s["s.file"]
    return (s["s.is_test"] == "False" and f.startswith("crates/") and "/src/" in f
            and s["s.kind"] in KINDS)


def is_main(s: dict[str, str]) -> bool:
    f = s["s.file"]
    return (s["s.kind"] == "Function" and s["s.path"].endswith("::main")
            and (f.endswith(("/src/main.rs", "/build.rs")) or "/src/bin/" in f))


def string_named(root: pathlib.Path) -> set[tuple[str, str]]:
    """What a string names, which cg has no edge for: a serde attribute's function, keyed
    `(file, fn)` when bare and `(crate dir, Type::fn)` when it names a type (`Vec::is_empty`
    names no method of ours), and a constant a format string captures, keyed `(crate dir, NAME)`."""
    found = set()
    for p in root.glob("crates/*/src/**/*.rs"):
        rel = p.relative_to(root)
        text, crate = p.read_text(errors="ignore"), "/".join(rel.parts[:2])
        for owner, name in SERDE_FN.findall(text):
            found.add((crate, f"{owner}::{name}") if owner else (str(rel), name))
        found |= {(crate, name) for name in CAPTURE.findall(text)}
        found |= {(str(rel), name) for name in ALLOCATOR.findall(text)}
    return found


def unreachable(symbols: list[dict[str, str]], edges: list[dict[str, str]],
                portal_words: set[str], named: set[tuple[str, str]] = frozenset()
                ) -> list[dict[str, str]]:
    syms = {s["s.id"]: s for s in symbols}
    out = collections.defaultdict(set)
    for e in edges:
        out[e["a.id"]].add(e["b.id"])
    impls = collections.defaultdict(set)
    for i, s in syms.items():
        if s["s.trait_name"] and s["s.self_type"]:
            # by the type's name alone: an impl may sit in another crate than its type, and cg
            # names a lifetime or generic type with its arguments (`Pending<'_>`)
            impls[s["s.self_type"].split("<", 1)[0]].add(i)
    entries = set()
    for i, s in syms.items():
        name = s["s.path"].rsplit("::", 1)[-1]
        crate = "/".join(s["s.file"].split("/")[:2])
        owned = f"{s['s.self_type']}::{name}" if s["s.self_type"] else name
        if is_main(s) or (production(s) and ((s["s.file"], name) in named or (crate, owned) in named)):
            entries.add(i)
        elif production(s) and s["s.package"] == SDK and s["s.kind"] == "Macro":
            entries.add(i)  # the SDK's one macro is `#[macro_export]`; cg sees it as private
        elif production(s) and s["s.visibility"] == "pub":
            if s["s.package"] == SDK or (
                    s["s.package"] in PORTAL_LINKS and name in portal_words
                    and (not s["s.self_type"] or s["s.self_type"] in portal_words)):
                entries.add(i)
    seen, stack = set(entries), list(entries)
    while stack:
        x = stack.pop()
        s = syms.get(x)
        nxt = set(out.get(x, ()))
        if s and s["s.kind"] in ("Struct", "Enum", "TypeAlias"):
            nxt |= impls.get(s["s.path"].rsplit("::", 1)[-1], set())
        for y in nxt - seen:
            seen.add(y)
            stack.append(y)
    rows = [s for i, s in syms.items() if i not in seen and production(s) and not is_main(s)]
    return sorted(rows, key=lambda s: (s["s.file"], int(s["s.line"] or 0)))


def words(src: pathlib.Path) -> set[str]:
    found: set[str] = set()
    for p in src.rglob("*.rs"):
        found |= set(re.findall(r"\b[A-Za-z_][A-Za-z0-9_]*\b", p.read_text(errors="ignore")))
    return found


def baseline(text: str) -> dict[str, int]:
    out = {}
    for line in text.splitlines():
        if line.strip() and not line.startswith("#"):
            name, count = (p.strip() for p in line.split("|"))
            out[name] = int(count)
    return out


def check(rows: list[dict[str, str]], allowed: dict[str, int]) -> list[str]:
    counts = collections.Counter(s["s.package"] for s in rows)
    problems = []
    for name in sorted(set(counts) | set(allowed)):
        have, may = counts.get(name, 0), allowed.get(name, 0)
        if have > may:
            problems.append(f"{name}: {have} unreachable, baseline {may}; new rows:")
            problems += [f"  {s['s.file']}:{s['s.line']} {s['s.kind']} {s['s.path']}"
                         for s in rows if s["s.package"] == name][:40]
        elif have < may:
            problems.append(f"{name}: {have} unreachable, baseline {may}: lower its line in "
                            f"{BASELINE.relative_to(ROOT)}")
    return problems


def selftest() -> None:
    def sym(i, path, kind="Function", pkg="context-gateway", file="crates/g/src/lib.rs",
            vis="private", trait="", self_type=""):
        return {"s.id": i, "s.path": path, "s.kind": kind, "s.package": pkg, "s.file": file,
                "s.line": "1", "s.loc": "3", "s.visibility": vis, "s.is_test": "False",
                "s.trait_name": trait, "s.self_type": self_type}
    symbols = [
        sym("m", "g::main", file="crates/g/src/main.rs"),
        sym("a", "g::used"),
        sym("t", "g::Thing", kind="Struct"),
        sym("f", "g::Thing::fmt", kind="Method", trait="Display", self_type="Thing"),
        sym("g", "g::Guard", kind="Struct"),
        sym("gd", "g::Guard::drop", kind="Method", trait="Drop", self_type="Guard<'_>"),
        sym("x", "g::cg_fixture_unreachable"),
        sym("k", "jc_core::Kind::new", kind="StaticMethod", pkg="jc-core", vis="pub",
            self_type="Kind"),
        sym("q", "jc_core::Kind::quiet", kind="Method", pkg="jc-core", vis="pub",
            self_type="Kind"),
        sym("s", "jc_app_sdk::open", pkg="jc-app-sdk", vis="pub"),
        sym("z", "g::helper", file="crates/g/tests/t.rs") | {"s.is_test": "True"},
        sym("fd", "g::Thing::mode", kind="Field"),
        sym("e", "g::Mode", kind="Enum"),
        sym("d", "g::default_mode"),
        sym("c", "g::LIMIT", kind="Constant", file="crates/g/src/page.rs"),
        sym("u", "g::Mode::is_unset", kind="Method", file="crates/g/src/mode.rs", self_type="Mode"),
        sym("v", "g::Thing::is_empty", kind="Method", self_type="Thing"),
    ]
    # main -> used -> Thing and Guard, whose Drop no call names; Thing holds `mode: Mode`
    edges = [{"a.id": "m", "b.id": "a"}, {"a.id": "a", "b.id": "t"}, {"a.id": "a", "b.id": "g"},
             {"a.id": "t", "b.id": "fd"}, {"a.id": "fd", "b.id": "e"}]
    rows = unreachable(symbols, edges, {"Kind", "new"}, {("crates/g/src/lib.rs", "default_mode"),
                                                       ("crates/g", "LIMIT"),
                                                       ("crates/g", "Mode::is_unset"),
                                                       ("crates/g", "Vec::is_empty")})
    assert SERDE_FN.findall('#[serde(default = "default_mode", skip_serializing_if = "x::is_unset")]') \
        == [("", "default_mode"), ("x", "is_unset")]
    assert SERDE_FN.findall('skip_serializing_if = "crate::kinds::Limits::is_unset"') == [("Limits", "is_unset")]
    assert SERDE_FN.findall('deserialize_with = "de_kind::<S, _>"') == [("", "de_kind")]
    assert ALLOCATOR.findall("#[global_allocator]\nstatic ALLOCATOR: Jemalloc = Jemalloc;") == ["ALLOCATOR"]
    assert CAPTURE.findall('format!("limit={LIMIT}&w={WIDTH:>4}", x = "{lower}")') == ["LIMIT", "WIDTH"]
    assert sorted(s["s.path"] for s in rows) == ["g::Thing::is_empty", "g::cg_fixture_unreachable",
                                                "jc_core::Kind::quiet"], rows
    allowed = {"context-gateway": 2, "jc-core": 1}
    assert check(rows, allowed) == []
    grown = check(rows, {"context-gateway": 1, "jc-core": 1})
    assert grown[0].startswith("context-gateway: 2 unreachable, baseline 1") and \
        "cg_fixture_unreachable" in grown[1], grown
    assert "lower its line" in check(rows, {"context-gateway": 3, "jc-core": 1})[0]
    assert table("graph: built | STALE: a.rs\n### Q1\na.id | b.id\n1 | 2\n") == [{"a.id": "1", "b.id": "2"}]
    try:
        table("hint: 'x' matches no Symbol\n")
        raise AssertionError("a hint must fail")
    except ValueError:
        pass
    print("cg-dead-code selftest: ok")


def query(statement: str) -> str:
    run = subprocess.run(["cg", "q", "--max-rows", "1000000", "--timeout", "300", "-e",
                          statement], cwd=ROOT, capture_output=True, text=True, check=False)
    if run.returncode != 0:
        raise SystemExit(run.stdout + run.stderr)
    return run.stdout


def main(argv: list[str]) -> int:
    if argv[1:] == ["--selftest"]:
        selftest()
        return 0
    args = argv[1:]
    portal = ROOT.parent / "joinedcontext-portal" / "src"
    if "--portal" in args:
        portal = pathlib.Path(args[args.index("--portal") + 1])
    if not portal.is_dir():
        print(f"no Portal source at {portal}: pass --portal", file=sys.stderr)
        return 2
    if "--from" in args:
        d = pathlib.Path(args[args.index("--from") + 1])
        texts = [(d / f).read_text() for f in ("symbols.txt", "edges.txt", "members.txt")]
    else:
        texts = [query(q) for q in (SYMBOLS, EDGES, MEMBERS)]
    sym_text, edge_text, member_text = texts
    rows = unreachable(table(sym_text), table(edge_text) + table(member_text), words(portal),
                       string_named(ROOT))
    counts = collections.Counter(s["s.package"] for s in rows)
    for name in sorted(counts):
        print(f"{name} | {counts[name]}")
    if "--rows" in args:
        for s in rows:
            print(f"{s['s.file']}:{s['s.line']} | {s['s.kind']} | {s['s.path']} | {s['s.loc']}")
    problems = check(rows, baseline(BASELINE.read_text()))
    for p in problems:
        print(p)
    print(f"cg-dead-code: {len(problems)} problem(s)" if problems else "cg-dead-code: OK")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
