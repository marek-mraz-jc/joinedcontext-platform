#!/usr/bin/env python3
"""Every edge between workspace crates follows the layers of docs Architecture/14-components.md
section 4 (T-4770).

Layers, bottom up: core (jc-core, then jc-manifest and jc-dcat on it), runtime (wasm-host,
functions), services (context-gateway, assistant, agent-proxy, token-sidecar), tools (jcctl,
camera-sim, jc-app-sdk). A normal or build dependency points only at a target in ALLOWED; a
dev-dependency may also reach a tool (a test fixture such as jcctl's bench_org) or the crate
itself (a test feature), never a crate that depends back on it. A member missing from LAYER fails,
so a new crate is placed before it lands. EXCEPTIONS only shrink.

    python3 scripts/ci/check-crate-layers.py [metadata.json]   (default: runs cargo metadata)
    python3 scripts/ci/check-crate-layers.py --selftest
"""

import json
import subprocess
import sys
from pathlib import Path

LAYER = {
    "jc-core": "core-base",
    "jc-manifest": "core",
    "jc-dcat": "core",
    "jc-relationships": "core",
    "wasm-host": "runtime",
    "functions": "runtime",
    "context-gateway": "service",
    "assistant": "service",
    "agent-proxy": "service",
    "token-sidecar": "service",
    "jcctl": "tool",
    "camera-sim": "tool",
    "jc-app-sdk": "tool",
}

# What a crate of a layer may depend on (normal and build dependencies).
ALLOWED = {
    "core-base": set(),
    "core": {"core-base"},
    "runtime": {"core-base", "core"},
    "service": {"core-base", "core", "runtime"},
    "tool": {"core-base", "core"},
}

# Known edges against the rule, each with the task that removes it.
EXCEPTIONS = {
    # public_dns (SSRF resolver) moves to jc_core::outbound in T-4776; diagnostics::redact
    # moves next to it (tasks/later/T-later-assistant-off-agent-proxy.md).
    ("assistant", "agent-proxy"),
}


def violations(metadata: dict) -> list[str]:
    members = {p["name"]: p for p in metadata["packages"]}
    out = []
    for name in sorted(members):
        if name not in LAYER:
            out.append(
                f"{name}: not placed in a layer; add it to LAYER and the architecture page"
            )
    normal = {
        (p["name"], d["name"])
        for p in members.values()
        for d in p["dependencies"]
        if d["name"] in members and d["kind"] != "dev"
    }
    for p in sorted(members.values(), key=lambda p: p["name"]):
        src = p["name"]
        if src not in LAYER:
            continue
        for d in sorted(p["dependencies"], key=lambda d: (d["name"], d["kind"] or "")):
            dst = d["name"]
            if dst not in members or dst not in LAYER:
                continue
            kind = d["kind"] or "normal"
            if (src, dst) in EXCEPTIONS:
                continue
            if kind == "dev":
                if dst == src or LAYER[dst] == "tool" and (dst, src) not in normal:
                    continue
                if (dst, src) in normal:
                    out.append(f"{src} -> {dst} (dev): a cycle, {dst} depends on {src}")
                    continue
            elif dst == src:
                out.append(f"{src} -> {dst} ({kind}): a crate depending on itself")
                continue
            if LAYER[dst] not in ALLOWED[LAYER[src]]:
                out.append(
                    f"{src} ({LAYER[src]}) -> {dst} ({LAYER[dst]}) [{kind}]: forbidden edge"
                )
    stale = sorted(e for e in EXCEPTIONS if e not in normal)
    out += [
        f"{a} -> {b}: exception no longer needed, remove it from EXCEPTIONS"
        for a, b in stale
    ]
    return out


def _meta(edges: dict) -> dict:
    return {
        "packages": [
            {"name": n, "dependencies": [{"name": d, "kind": k} for d, k in deps]}
            for n, deps in edges.items()
        ]
    }


def selftest() -> None:
    good = _meta(
        {
            "jc-core": [],
            "jc-manifest": [("jc-core", None)],
            "context-gateway": [
                ("jc-core", None),
                ("jc-manifest", None),
                ("jcctl", "dev"),
            ],
            "jcctl": [("jc-core", None), ("jc-manifest", None)],
            "agent-proxy": [("jc-core", None), ("agent-proxy", "dev")],
            "assistant": [("agent-proxy", None)],
        }
    )
    assert violations(good) == [], violations(good)
    bad = _meta(
        {
            "jc-core": [("context-gateway", None)],
            "context-gateway": [("jc-core", None), ("token-sidecar", None)],
            "token-sidecar": [],
            "jcctl": [("context-gateway", "build")],
            "jc-manifest": [("jc-dcat", None)],
            "jc-dcat": [("jc-core", None)],
            "new-crate": [],
        }
    )
    got = violations(bad)
    want = [
        "new-crate: not placed in a layer; add it to LAYER and the architecture page",
        "context-gateway (service) -> token-sidecar (service) [normal]: forbidden edge",
        "jc-core (core-base) -> context-gateway (service) [normal]: forbidden edge",
        "jc-manifest (core) -> jc-dcat (core) [normal]: forbidden edge",
        "jcctl (tool) -> context-gateway (service) [build]: forbidden edge",
        "assistant -> agent-proxy: exception no longer needed, remove it from EXCEPTIONS",
    ]
    assert got == want, got
    cycle = _meta(
        {
            "jc-core": [],
            "jcctl": [("jc-core", None), ("jc-manifest", "dev")],
            "jc-manifest": [("jc-core", None), ("jcctl", None)],
            "assistant": [("agent-proxy", None)],
            "agent-proxy": [],
        }
    )
    assert (
        "jcctl -> jc-manifest (dev): a cycle, jc-manifest depends on jcctl"
        in violations(cycle)
    )


if __name__ == "__main__":
    selftest()
    if "--selftest" in sys.argv:
        print("check-crate-layers: selftest ok")
        sys.exit(0)
    if len(sys.argv) > 1:
        raw = Path(sys.argv[1]).read_text()
    else:
        raw = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    problems = violations(json.loads(raw))
    for p in problems:
        print(f"::error::{p}")
    if problems:
        sys.exit(1)
    print(f"check-crate-layers: {len(LAYER)} crates, every edge follows the layers")
