#!/usr/bin/env python3
"""Every file a crate embeds reaches the image build (T-4267).

`include_str!`/`include_bytes!` read a file at compile time. The image build sees only what
.dockerignore lets into the context, so an embedded file it excludes compiles everywhere except
there: crates/context-gateway/src/mcp/primer.md under `**/*.md` broke image run 38858239805,
and nothing in the fast lane said so. Literal paths only; a path a build script assembles is
its own concern.

    python3 scripts/ci/check-dockerignore-includes.py [repo root]
"""
import pathlib
import re
import sys

INCLUDE = re.compile(r'include_(?:str|bytes)!\(\s*"([^"]+)"\s*\)')


def pattern(rule: str) -> re.Pattern[str]:
    """A .dockerignore rule as a regex over a slash path: `**` any depth, `*` and `?` within a
    segment; a rule names a file or everything below a directory."""
    out, i = "", 0
    rule = rule.strip("/")
    while i < len(rule):
        if rule.startswith("**/", i):
            out, i = out + "(?:.*/)?", i + 3
        elif rule.startswith("**", i):
            out, i = out + ".*", i + 2
        elif rule[i] == "*":
            out, i = out + "[^/]*", i + 1
        elif rule[i] == "?":
            out, i = out + "[^/]", i + 1
        else:
            out, i = out + re.escape(rule[i]), i + 1
    return re.compile(f"{out}(?:/.*)?")


def excluded(path: str, rules: list[str]) -> bool:
    """Docker's order: the last rule that matches decides, `!` lets the path back in."""
    out = False
    for rule in rules:
        negated = rule.startswith("!")
        if pattern(rule.lstrip("!")).fullmatch(path):
            out = not negated
    return out


def rules_of(text: str) -> list[str]:
    return [l.strip() for l in text.splitlines() if l.strip() and not l.lstrip().startswith("#")]


def problems(root: pathlib.Path) -> list[str]:
    rules = rules_of((root / ".dockerignore").read_text())
    out = []
    for source in sorted((root / "crates").rglob("*.rs")):
        rel_source = source.relative_to(root).as_posix()
        if excluded(rel_source, rules):
            continue
        for match in INCLUDE.finditer(source.read_text()):
            target = (source.parent / match.group(1)).resolve().relative_to(root.resolve()).as_posix()
            if excluded(target, rules):
                out.append(f"{rel_source}: embeds {target}, which .dockerignore keeps out of the image build")
    return out


def selftest() -> None:
    rules = rules_of("target/\n**/*.md\n# kept\n!crates/assistant/guide/*.md\n!crates/*/src/**/*.md\nschemas/\n")
    assert excluded("README.md", rules)
    assert excluded("docs/a/b.md", rules)
    assert not excluded("crates/assistant/guide/x.md", rules)
    assert not excluded("crates/context-gateway/src/mcp/primer.md", rules)
    assert excluded("crates/context-gateway/README.md", rules)
    assert excluded("target/release/x", rules)
    assert excluded("schemas/kinds/a.json", rules)
    assert not excluded("crates/jcctl/templates/org/policies/roles.rego", rules)
    assert excluded("crates/x/src/primer.md", rules_of("**/*.md\n"))


if __name__ == "__main__":
    selftest()
    found = problems(pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "."))
    for problem in found:
        print(problem, file=sys.stderr)
    sys.exit(1 if found else 0)
