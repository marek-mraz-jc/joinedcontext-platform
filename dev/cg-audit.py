#!/usr/bin/env python3
"""One audit across the code graphs of the platform, the Portal and Antares (T-4594).

cg keeps one repository per graph. This script asks each clone's graph two things and joins
the answers:

- HTTP links: every `PROVIDES` and `USES` of the `http` channel (the routes and the pinned
  client hops of each repo's `.cg/links/*.toml`), matched across repositories on the channel
  key. A use another repository serves is a link; a use nobody serves is unmatched (a
  renamed route, a removed handler, a stale pin); a route nobody calls is listed, as most of
  those are external by design (OIDC, webhooks, MCP, probes).
- dev/cg-audit.cypher: named statements for security, modularity and memory, each counted per
  repository (a GREP hit counts once per enclosing function, tests and comments never).

ci/cg-audit-baseline.txt holds `repo | name | count`, the unmatched links among them. A count
above its line is red; a count below it is red until the line is lowered, so the baseline
only goes down. A repository without a graph is skipped and said so. cg is not on the CI
runner: tasks/push runs this beside dev/cg-structure.py, checking the platform's own lines.

  dev/cg-audit.py                     audit every clone found, check all their lines
  dev/cg-audit.py --check platform    check only that repository's lines
  dev/cg-audit.py --repo portal=DIR   another clone (repeatable)
  dev/cg-audit.py --write             also write docs/analysis/cg-audit.md
  dev/cg-audit.py --selftest
"""

from __future__ import annotations

import importlib.util
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
QUERIES = ROOT / "dev" / "cg-audit.cypher"
BASELINE = ROOT / "ci" / "cg-audit-baseline.txt"
REPORT = ROOT / "docs" / "analysis" / "cg-audit.md"
REPOS = {
    "platform": ROOT,
    "portal": ROOT.parent / "joinedcontext-portal",
    "antares": ROOT.parent / "Antares-NGSI-LD-Context-Broker",
}
_spec = importlib.util.spec_from_file_location("cg_invariants", ROOT / "dev" / "cg-invariants.py")
_inv = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_inv)
sections = _inv.sections

UNMATCHED = "links-unmatched"
LINKS = (
    "// provides: the routes of the graph\n"
    "MATCH (s)-[r:PROVIDES]->(c:Channel) WHERE c.channel =~ 'http' "
    "RETURN c.key, s.path, r.file, r.line;\n"
    "// uses: the client calls of the graph\n"
    "MATCH (s)-[r:USES]->(c:Channel) WHERE c.channel =~ 'http' "
    "RETURN c.key, s.path, r.file, r.line;\n"
)


def cg(repo: pathlib.Path, statements: str) -> str:
    run = subprocess.run(["cg", "q", "--max-rows", "1000000", "--timeout", "300", "-"],
                         input=statements, cwd=repo, capture_output=True, text=True, check=False)
    if run.returncode != 0:
        raise SystemExit(f"cg q in {repo}: {run.stdout}{run.stderr}")
    return run.stdout


def key(raw: str) -> tuple[str, str]:
    """`VERB /path` with every path parameter as `{}`."""
    verb, _, path = raw.partition(" ")
    return verb.upper(), re.sub(r"\{[^}]*\}", "{}", path.split("?", 1)[0])


def serves(provided: str, used: str) -> bool:
    """A route answers a call with its verb (or ANY) on the same path; an ANY route ending in
    `/{}` is a catch-all and answers everything below it (antares_views::Views::router)."""
    (pv, pp), (uv, up) = key(provided), key(used)
    if pv not in (uv, "ANY"):
        return False
    return pp == up or (pv == "ANY" and pp.endswith("/{}") and up.startswith(pp[:-2]))


def split(rows: list[str]) -> list[tuple[str, str, str]]:
    """`(key, symbol, file:line)` of `key | path | file | line` rows."""
    out = []
    for row in rows:
        cells = row.split(" | ")
        out.append((cells[0], cells[1], f"{cells[2]}:{cells[3]}" if len(cells) > 3 else ""))
    return out


def join(found: dict[str, dict[str, list[str]]]) -> tuple[list[str], list[str], list[str]]:
    """Links, unmatched uses and routes nobody calls, from each repo's provides and uses."""
    provides = {r: split(f.get("provides", [])) for r, f in found.items()}
    uses = {r: split(f.get("uses", [])) for r, f in found.items()}
    links, unmatched, called = [], [], set()
    for repo, calls in uses.items():
        for used, caller, at in dict.fromkeys(calls):
            own = [p for p in provides[repo] if serves(p[0], used)]
            other = [(r, p) for r, ps in provides.items() if r != repo for p in ps
                     if serves(p[0], used)]
            called.update((repo, p) for p in own)
            called.update(other)
            links += [f"{repo}: {caller} -> {r}: {p[1]} | {used}" for r, p in other]
            if not own and not other:
                unmatched.append(f"{repo} | {used} | {caller} | {at}")
    uncalled = [f"{r} | {p[0]} | {p[1]}" for r, ps in provides.items() for p in dict.fromkeys(ps)
                if (r, p) not in called]
    return sorted(set(links)), sorted(unmatched), sorted(uncalled)


VENDORED = re.compile(r"^vendor/\S*:\d+ \|.*\n?", re.MULTILINE)


def counts(output: str) -> tuple[dict[str, int], list[str]]:
    """Distinct rows per named statement; cg's own warnings as problems. A GREP hit under
    vendor/ (a patched third-party crate: platform kreuzberg, Antares rumqttc) is not ours."""
    found = sections(VENDORED.sub("", output))
    problems = found.pop("!", [])
    return {name: len(dict.fromkeys(rows)) for name, rows in found.items()}, problems


def baseline(text: str) -> dict[tuple[str, str], int]:
    out = {}
    for line in text.splitlines():
        if line.strip() and not line.startswith("#"):
            repo, name, n = (p.strip() for p in line.split("|"))
            out[(repo, name)] = int(n)
    return out


def check(have: dict[tuple[str, str], int], allowed: dict[tuple[str, str], int],
          repos: set[str]) -> list[str]:
    problems = []
    for repo, name in sorted(set(have) | set(allowed)):
        if repo not in repos:
            continue
        n, may = have.get((repo, name), 0), allowed.get((repo, name), 0)
        if n > may:
            problems.append(f"{repo} | {name}: {n}, baseline {may}: fix the new rows, or raise "
                            f"the line with the reason in the commit")
        elif n < may:
            problems.append(f"{repo} | {name}: {n}, baseline {may}: lower its line in "
                            f"{BASELINE.relative_to(ROOT)}")
    return problems


def report(heads: dict[str, str], have: dict[tuple[str, str], int], links: list[str],
           unmatched: list[str], uncalled: list[str], skipped: list[str]) -> str:
    names = sorted({n for _, n in have})
    repos = sorted({r for r, _ in have})
    lines = ["# Code-graph audit: platform, Portal and Antares", "",
             "Generated by `dev/cg-audit.py --write` (T-4594) from each clone's code graph. Do not "
             "edit by hand: change the code, `dev/cg-audit.cypher` or a repo's `.cg/links/*.toml`, "
             "then run the script.", "", "Graphs:", ""]
    lines += [f"- {r}: `{h}`" for r, h in sorted(heads.items())]
    lines += [f"- {s}: no graph, skipped" for s in skipped]
    lines += ["", "## Counts", "", "| query | " + " | ".join(repos) + " |",
              "|---|" + "---|" * len(repos)]
    lines += [f"| {n} | " + " | ".join(str(have.get((r, n), 0)) for r in repos) + " |"
              for n in names]
    lines += ["", "## Links across repositories", ""] + [f"- {link}" for link in links or ["none"]]
    lines += ["", "## Calls no repository serves", ""] + [f"- {u}" for u in unmatched or ["none"]]
    lines += ["", f"## Routes no graph calls ({len(uncalled)})", "",
              "Most are called from outside the graphs (browsers, OIDC, webhooks, MCP clients, "
              "probes); the Portal backend's calls to the gateway and the broker are not yet "
              "declared as `uses` in its `.cg/links`.", ""]
    lines += [f"- {u}" for u in uncalled]
    return "\n".join(lines) + "\n"


def audit(repos: dict[str, pathlib.Path], run=cg):
    queries = QUERIES.read_text()
    have, problems, found, heads, skipped = {}, [], {}, {}, []
    for name, path in repos.items():
        if not (path / ".cg" / "db").is_dir():
            skipped.append(name)
            continue
        output = run(path, queries)
        heads[name] = output.split("\n", 1)[0].removeprefix("graph: ").split(" | STALE")[0]
        n, warned = counts(output)
        have.update({(name, q): c for q, c in n.items()})
        problems += [f"{name} | {w}" for w in warned]
        found[name] = sections(run(path, LINKS))
    links, unmatched, uncalled = join(found)
    for name in found:
        have[(name, UNMATCHED)] = sum(1 for u in unmatched if u.startswith(f"{name} | "))
    return have, problems, heads, skipped, (links, unmatched, uncalled)


def selftest() -> None:
    assert serves("ANY /x/views/{}", "GET /x/views/{view}/ngsi-ld/v1/entities")
    assert serves("GET /ngsi-ld/v1/entities", "GET /ngsi-ld/v1/entities?type=X")
    assert not serves("POST /ngsi-ld/v1/entities", "GET /ngsi-ld/v1/entities")
    assert not serves("GET /x/views/{}", "GET /x/views/{}/ngsi-ld/v1/entities")
    assert serves("PATCH /e/{}/attrs", "PATCH /e/{id}/attrs")

    # two tiny repos: one route, one call that reaches it, one call nobody serves
    graphs = {
        "a": {"queries": "graph: built A\n### Q1: memory-body-copy: x\n2 hits in 1 files\n"
                         "at | in | test | comment | text\nf.rs:1 | a::f | False | False | b\n"
                         "f.rs:2 | a::f | False | False | b\nt.rs:3 | a::t | True | False | b\n"
                         "vendor/k/src/x.rs:4 | k::x | False | False | b\n",
              "links": "graph: built A\n### Q1: provides: r\n1 rows\nc.key | s.path | r.file | r.line\n"
                       "GET /api/{} | a::serve | f.rs | 9\n"
                       "### Q2: uses: u\n0 rows\nc.key | s.path | r.file | r.line\n"},
        "b": {"queries": "graph: built B\n### Q1: memory-body-copy: x\n0 hits in 0 files\n",
              "links": "graph: built B\n### Q1: provides: r\n0 rows\nc.key | s.path | r.file | r.line\n"
                       "### Q2: uses: u\n2 rows\nc.key | s.path | r.file | r.line\n"
                       "GET /api/{id} | b::call | c.rs | 4\nPOST /gone | b::old | c.rs | 7\n"},
    }
    tmp = pathlib.Path(__import__("tempfile").mkdtemp())
    for name in ("a", "b"):
        (tmp / name / ".cg" / "db").mkdir(parents=True)

    def fake(path: pathlib.Path, statements: str) -> str:
        return graphs[path.name]["links" if statements == LINKS else "queries"]

    have, problems, heads, skipped, (links, unmatched, uncalled) = audit(
        {"a": tmp / "a", "b": tmp / "b", "c": tmp / "c"}, run=fake)
    assert problems == [] and skipped == ["c"] and heads == {"a": "built A", "b": "built B"}
    assert links == ["b: b::call -> a: a::serve | GET /api/{id}"], links
    assert unmatched == ["b | POST /gone | b::old | c.rs:7"], unmatched
    assert uncalled == [], uncalled
    assert have == {("a", "memory-body-copy"): 1, ("b", "memory-body-copy"): 0,
                    ("a", UNMATCHED): 0, ("b", UNMATCHED): 1}, have
    # the ratchet: equal is green, a rise red, a fall red until lowered, other repos ignored
    allowed = baseline("# c\na | memory-body-copy | 1\nb | links-unmatched | 1\n")
    assert check(have, allowed, {"a", "b"}) == []
    assert check(have | {("a", "memory-body-copy"): 2}, allowed, {"a", "b"})[0].startswith(
        "a | memory-body-copy: 2, baseline 1: fix")
    assert check(have | {("b", UNMATCHED): 0}, allowed, {"a", "b"}) == [
        "b | links-unmatched: 0, baseline 1: lower its line in ci/cg-audit-baseline.txt"]
    assert check(have | {("b", UNMATCHED): 5}, allowed, {"a"}) == []
    # cg's own warning is a problem, never a count
    _, warned = counts(graphs["a"]["queries"] + "hint: 'x' matches no Symbol\n")
    assert warned == ["memory-body-copy: hint: 'x' matches no Symbol"], warned
    text = report(heads, have, links, unmatched, uncalled, skipped)
    assert "| memory-body-copy | 1 | 0 |" in text and "- c: no graph, skipped" in text, text
    print("cg-audit selftest: ok")


def main(argv: list[str]) -> int:
    args = argv[1:]
    if args == ["--selftest"]:
        selftest()
        return 0
    repos = dict(REPOS)
    for i, a in enumerate(args):
        if a == "--repo" and i + 1 < len(args) and "=" in args[i + 1]:
            name, path = args[i + 1].split("=", 1)
            repos[name] = pathlib.Path(path)
    have, problems, heads, skipped, (links, unmatched, uncalled) = audit(repos)
    only = {args[args.index("--check") + 1]} if "--check" in args else set(heads)
    problems += check(have, baseline(BASELINE.read_text()), only)
    for repo, name in sorted(have):
        print(f"{repo} | {name} | {have[(repo, name)]}")
    for s in skipped:
        print(f"{s}: no graph at {repos[s]}, skipped")
    for u in unmatched:
        print(f"unmatched | {u}")
    if "--write" in args:
        REPORT.write_text(report(heads, have, links, unmatched, uncalled, skipped))
    for p in problems:
        print(p)
    print(f"cg-audit: {len(problems)} problem(s)" if problems else "cg-audit: OK")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
