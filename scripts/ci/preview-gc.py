#!/usr/bin/env python3
"""Delete branch preview images older than 14 days from ghcr.io (T-5547, Deployment/17).

    preview-gc.py <owner> <package>... [--days 14] [--dry-run]
    preview-gc.py --selftest

A container version goes when every tag it carries starts with `pv-` (preview.yml pushes only
such tags) and it was last updated more than --days ago, together with the cosign signature
versions of its digest (`sha256-<hex>.sig`, `.att`). A version with any other tag (`main`,
`dev-<N>`, a commit sha, a cosign tag of a kept digest) and every untagged version stay: an
untagged version can be a platform manifest of a pinned index, and a wrong delete takes a running
image away. ponytail: the untagged children of a deleted preview index stay too; GitHub's own
untagged-version cleanup is the upgrade when they add up.
`--dry-run` prints what would go and deletes nothing. Needs GH_TOKEN with packages: write on the
packages (the repository that pushes them holds admin on them).
A copy lives in joinedcontext-platform and joinedcontext-portal; keep the two equal.
"""
import argparse
import json
import re
import subprocess
import sys
from datetime import UTC, datetime, timedelta
from urllib.parse import quote

NAME = re.compile(r"^[A-Za-z0-9_.-]+(/[A-Za-z0-9_.-]+)*$")
DIGEST = re.compile(r"^sha256:([0-9a-f]{64})$")


def doomed(versions, now, days):
    """The ids of `versions` ({id, name, updated_at, tags}) to delete, each with the reason."""
    cutoff = now - timedelta(days=days)
    out, gone = [], set()
    for v in versions:
        tags = v["tags"] or []
        old = datetime.fromisoformat(v["updated_at"]) < cutoff
        if tags and old and all(t.startswith("pv-") for t in tags):
            out.append((v["id"], f"{v['name']} {','.join(tags)} updated {v['updated_at']}"))
            m = DIGEST.match(v["name"])
            if m:
                gone.add(f"sha256-{m.group(1)}")
    for v in versions:
        tags = v["tags"] or []
        if tags and all(t.rsplit(".", 1)[0] in gone and t.endswith((".sig", ".att")) for t in tags):
            out.append((v["id"], f"{v['name']} {','.join(tags)} (cosign of a deleted preview)"))
    return out


def gh(*args):
    result = subprocess.run(["gh", "api", *args], capture_output=True, text=True, check=False)
    if result.returncode != 0:
        sys.exit(f"preview-gc: gh api {' '.join(args)} failed: {result.stderr.strip()} (the token needs "
                 "packages: write and the package must grant this repository admin access)")
    return result.stdout


def versions(owner, package):
    out = gh("--paginate", f"/users/{owner}/packages/container/{quote(package, safe='')}/versions?per_page=100",
             "--jq", ".[] | {id, name, updated_at, tags: .metadata.container.tags}")
    return [json.loads(line) for line in out.splitlines() if line.strip()]


def delete(owner, package, version_id):
    gh("-X", "DELETE", f"/users/{owner}/packages/container/{quote(package, safe='')}/versions/{version_id}")


def selftest():
    now = datetime(2026, 10, 25, tzinfo=UTC)
    old, new = "2026-10-01T00:00:00Z", "2026-10-20T00:00:00Z"
    a, b, c = "a" * 64, "b" * 64, "c" * 64
    vs = [
        {"id": 1, "name": f"sha256:{a}", "updated_at": old, "tags": ["pv-map-legend-0123456789ab"]},
        {"id": 2, "name": f"sha256:{b}", "updated_at": new, "tags": ["pv-map-legend-ba9876543210"]},
        {"id": 3, "name": f"sha256:{c}", "updated_at": old, "tags": ["pv-x-0123456789ab", "main"]},
        {"id": 4, "name": "sha256:" + "d" * 64, "updated_at": old, "tags": [f"sha256-{a}.sig"]},
        {"id": 5, "name": "sha256:" + "e" * 64, "updated_at": old, "tags": [f"sha256-{b}.sig"]},
        {"id": 6, "name": "sha256:" + "f" * 64, "updated_at": old, "tags": []},
        {"id": 7, "name": "sha256:" + "1" * 64, "updated_at": old, "tags": ["dev-41", "0" * 40]},
        {"id": 8, "name": "sha256:" + "2" * 64, "updated_at": old, "tags": [f"sha256-{c}.att"]},
    ]
    ids = [i for i, _ in doomed(vs, now, 14)]
    assert ids == [1, 4], f"only the old pv- image and its signature go, got {ids}"
    assert [i for i, _ in doomed(vs, now, 30)] == [], "nothing is older than 30 days"
    assert [i for i, _ in doomed(vs, now, 0)] == [1, 2, 4, 5], "--days 0 takes every pv-only image"
    print("preview-gc selftest: ok")


def main(argv):
    if argv == ["--selftest"]:
        return selftest()
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("owner")
    p.add_argument("packages", nargs="+")
    p.add_argument("--days", type=int, default=14)
    p.add_argument("--dry-run", action="store_true")
    args = p.parse_args(argv)
    if args.days < 1 or not NAME.match(args.owner) or not all(NAME.match(n) for n in args.packages):
        sys.exit("preview-gc: --days must be at least 1, and owner and packages ghcr.io names")
    now = datetime.now(UTC)
    for package in args.packages:
        gone = doomed(versions(args.owner, package), now, args.days)
        for version_id, why in gone:
            print(f"{'would delete' if args.dry_run else 'delete'} {package} {why}")
            if not args.dry_run:
                delete(args.owner, package, version_id)
        print(f"{package}: {len(gone)} version(s) {'to delete' if args.dry_run else 'deleted'}")


if __name__ == "__main__":
    main(sys.argv[1:])
