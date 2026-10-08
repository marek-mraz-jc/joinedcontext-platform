#!/usr/bin/env python3
"""Every workspace member is in the Dockerfile's dependency layer (T-3366).

The image build copies each member's Cargo.toml and stubs its sources before `cargo build`, so the
dependencies are a layer of their own. A member added to Cargo.toml and not here breaks the image
build, as crates/app-sdk did on main (image run 37757792335), and nothing in the fast lane said so.

    python3 scripts/ci/check-dockerfile-members.py [Cargo.toml] [Dockerfile]
"""
import re
import sys
import tomllib


def missing(cargo: str, dockerfile: str) -> list[str]:
    members = tomllib.loads(cargo)["workspace"]["members"]
    out = []
    for member in members:
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


if __name__ == "__main__":
    selftest()
    cargo_path, docker_path = (sys.argv[1:3] + ["Cargo.toml", "Dockerfile"][len(sys.argv[1:3]):])[:2]
    problems = missing(open(cargo_path).read(), open(docker_path).read())
    for problem in problems:
        print(problem, file=sys.stderr)
    sys.exit(1 if problems else 0)
