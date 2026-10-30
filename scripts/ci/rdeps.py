#!/usr/bin/env python3
"""The workspace crates a change reaches (T-6153), for ci.yml's rust job.

    python3 scripts/ci/rdeps.py all | <crate folder>... >> "$GITHUB_ENV"
    python3 scripts/ci/rdeps.py --selftest

scripts/ci/changes.sh names the folders under crates/ a change touched (`crates=`), or `all`. This
adds every workspace crate that depends on one of them, directly or not, normal, build or dev
dependency alike, and prints three lines for the job's environment:

    CRATES=" a b "                  the set, space-padded for contains(); `all` for the workspace
    CARGO_PKGS=-p a -p b            for clippy and rustdoc; --workspace for all
    CARGO_TEST_PKGS=-p a            for `cargo test --lib --bins` without the assistant, which is
                                    tested on its own (ci.yml says why); empty when none is left

A folder that is no workspace member (a new crate the workspace does not list yet) checks the
whole workspace. ci-full still runs the whole workspace every day.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


def reach(meta: dict, folders: list[str]) -> list[str] | None:
    """The package names the folders and their dependents make up, or None for the workspace."""
    if not folders or "all" in folders:
        return None
    members = {p["name"]: p for p in meta["packages"]}
    by_folder = {Path(p["manifest_path"]).parent.name: p["name"] for p in members.values()}
    if any(f not in by_folder for f in folders):
        return None
    dependents: dict[str, set[str]] = {name: set() for name in members}
    for p in members.values():
        for d in p["dependencies"]:
            if d.get("path") and d["name"] in members and d["name"] != p["name"]:
                dependents[d["name"]].add(p["name"])
    todo = [by_folder[f] for f in folders]
    seen: set[str] = set()
    while todo:
        name = todo.pop()
        if name not in seen:
            seen.add(name)
            todo.extend(dependents[name])
    return sorted(seen)


def lines(names: list[str] | None) -> str:
    if names is None:
        return "CRATES=all\nCARGO_PKGS=--workspace\nCARGO_TEST_PKGS=--workspace --exclude assistant\n"
    def flags(ns) -> str:
        return " ".join(f"-p {n}" for n in ns)
    return (f"CRATES= {' '.join(names)} \nCARGO_PKGS={flags(names)}\n"
            f"CARGO_TEST_PKGS={flags(n for n in names if n != 'assistant')}\n")


def selftest() -> None:
    def pkg(name: str, folder: str, *deps: str) -> dict:
        return {"name": name, "manifest_path": f"/w/crates/{folder}/Cargo.toml",
                "dependencies": [{"name": d, "path": f"/w/crates/{d}"} for d in deps]
                + [{"name": "serde", "path": None}]}
    meta = {"packages": [pkg("jc-core", "jc-core"), pkg("jc-manifest", "jc-manifest", "jc-core"),
                         pkg("jcctl", "jcctl", "jc-core", "jc-manifest"),
                         pkg("context-gateway", "context-gateway", "jc-core", "jcctl"),
                         pkg("assistant", "assistant", "jc-manifest"),
                         pkg("jc-app-sdk", "app-sdk"), pkg("agent-proxy", "agent-proxy", "agent-proxy")]}
    cases = [
        (["context-gateway"], ["context-gateway"]),
        (["jcctl"], ["context-gateway", "jcctl"]),
        (["jc-manifest"], ["assistant", "context-gateway", "jc-manifest", "jcctl"]),
        (["app-sdk"], ["jc-app-sdk"]),
        (["agent-proxy"], ["agent-proxy"]),
        (["jc-core"], ["assistant", "context-gateway", "jc-core", "jc-manifest", "jcctl"]),
        (["all"], None), ([], None), (["new-crate"], None), (["jcctl", "new-crate"], None),
    ]
    for folders, want in cases:
        got = reach(meta, folders)
        assert got == want, f"{folders}: got {got}, want {want}"
    assert lines(["assistant"]) == "CRATES= assistant \nCARGO_PKGS=-p assistant\nCARGO_TEST_PKGS=\n"
    assert lines(["jcctl", "context-gateway"]).splitlines()[1] == "CARGO_PKGS=-p jcctl -p context-gateway"
    assert lines(None).startswith("CRATES=all\nCARGO_PKGS=--workspace\n")
    print("rdeps selftest: ok")


def main(argv: list[str]) -> int:
    if argv == ["--selftest"]:
        selftest()
        return 0
    folders = " ".join(argv).split()
    meta = None
    if folders and "all" not in folders:
        meta = json.loads(subprocess.run(["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
                                         check=True, capture_output=True, text=True).stdout)
    sys.stdout.write(lines(reach(meta, folders) if meta else None))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
