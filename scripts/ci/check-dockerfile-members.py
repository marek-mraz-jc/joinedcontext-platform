#!/usr/bin/env python3
"""Every workspace member is in the Dockerfile's dependency layer (T-3366).

The image build copies each member's Cargo.toml and stubs its sources before `cargo build`, so the
dependencies are a layer of their own. A member added to Cargo.toml and not here breaks the image
build, as crates/app-sdk did on main (image run 37757792335), and nothing in the fast lane said so.
A target a member names in its Cargo.toml ([[bin]], [[test]], [[bench]], [[example]]) must exist for
cargo to read that manifest at all, so the stub build writes a file at its path too: the
`[[bench]] pdp_decision` of context-gateway broke the image build that way (ship run 39893726013).

    python3 scripts/ci/check-dockerfile-members.py [Cargo.toml] [Dockerfile]
"""
import re
import sys
import tomllib
from pathlib import Path

DEFAULT_DIRS = {"bin": "src/bin", "test": "tests", "bench": "benches", "example": "examples"}


def target_paths(manifest: str) -> list[str]:
    """Paths of the targets a member's Cargo.toml names explicitly, relative to the member."""
    parsed = tomllib.loads(manifest)
    return [
        target.get("path", f"{folder}/{target['name']}.rs")
        for kind, folder in DEFAULT_DIRS.items()
        for target in parsed.get(kind, [])
    ]


def missing(cargo: str, dockerfile: str, manifests: dict[str, str] | None = None) -> list[str]:
    members = tomllib.loads(cargo)["workspace"]["members"]
    out = []
    for member in members:
        for path in target_paths((manifests or {}).get(member, "")):
            if f"> {member}/{path}" not in dockerfile:
                out.append(f"{member}: the stub build writes no `{member}/{path}` its Cargo.toml names")
        if f"COPY {member}/Cargo.toml {member}/Cargo.toml" not in dockerfile:
            out.append(f"{member}: no `COPY {member}/Cargo.toml` in the dependency layer")
        if not re.search(rf"mkdir -p [^\n]*\b{re.escape(member)}/src\b", dockerfile):
            out.append(f"{member}: its src/ is not created for the stub build")
    return out


def selftest() -> None:
    cargo = '[workspace]\nmembers = ["crates/a", "crates/b"]\n'
    good = "COPY crates/a/Cargo.toml crates/a/Cargo.toml\nCOPY crates/b/Cargo.toml crates/b/Cargo.toml\nRUN mkdir -p crates/a/src crates/b/src \\\n"
    assert missing(cargo, good) == [], missing(cargo, good)
    bad = "COPY crates/a/Cargo.toml crates/a/Cargo.toml\nRUN mkdir -p crates/a/src \\\n"
    assert missing(cargo, bad) == [
        "crates/b: no `COPY crates/b/Cargo.toml` in the dependency layer",
        "crates/b: its src/ is not created for the stub build",
    ], missing(cargo, bad)
    bench = {"crates/a": '[[bench]]\nname = "speed"\nharness = false\n'}
    assert missing(cargo, good, bench) == [
        "crates/a: the stub build writes no `crates/a/benches/speed.rs` its Cargo.toml names",
    ], missing(cargo, good, bench)
    stubbed = good + " && echo 'fn main() {}' > crates/a/benches/speed.rs \\\n"
    assert missing(cargo, stubbed, bench) == [], missing(cargo, stubbed, bench)
    binary = {"crates/b": '[[bin]]\nname = "b"\npath = "src/bin/b.rs"\n'}
    assert missing(cargo, good, binary) == [
        "crates/b: the stub build writes no `crates/b/src/bin/b.rs` its Cargo.toml names",
    ], missing(cargo, good, binary)


if __name__ == "__main__":
    selftest()
    cargo_path, docker_path = (sys.argv[1:3] + ["Cargo.toml", "Dockerfile"][len(sys.argv[1:3]):])[:2]
    cargo_text = Path(cargo_path).read_text()
    root = Path(cargo_path).parent
    manifests = {
        member: (root / member / "Cargo.toml").read_text()
        for member in tomllib.loads(cargo_text)["workspace"]["members"]
    }
    problems = missing(cargo_text, Path(docker_path).read_text(), manifests)
    for problem in problems:
        print(problem, file=sys.stderr)
    sys.exit(1 if problems else 0)
