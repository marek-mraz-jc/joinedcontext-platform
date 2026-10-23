#!/usr/bin/env python3
"""Hourly gate of image.yml (T-5449): print the commit of the newest green image.yml run on main,
after waiting until GAP seconds have passed since that commit was built.

    image-gate.py <owner/repo> <sha> <gap-seconds>
    image-gate.py --selftest

A run that builds after a finished ci on main calls this first, so main gets at most one image an
hour (T-4696's cost) and the newest merge still gets one within the hour, even when no later merge
or cron comes: the workflow's gate concurrency group holds one waiting run and replaces the pending
one. The wait counts from the oldest green run of the newest built commit (later green runs of the
same commit built nothing), at the moment its `changes` job decided, which is after its own wait.
Prints nothing when main has no green run yet; the caller then builds everything.
"""
import json
import re
import subprocess
import sys
import time
from datetime import datetime

SHA = re.compile(r"^[0-9a-f]{40}$")
REPO = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
# Green runs read per call: the streak of one commit is rarely longer (cron + ci + ci-full).
RUNS = 10


def wait_seconds(sha, runs, now, gap):
    """Seconds to wait before building `sha`. `runs`: (head_sha, decided_epoch) of green runs on
    main, newest first."""
    if not runs or runs[0][0] == sha:
        return 0
    first = runs[0][1]
    for s, t in runs:
        if s != runs[0][0]:
            break
        first = min(first, t)
    return max(0, int(first + gap - now))


def epoch(stamp):
    return datetime.fromisoformat(stamp).timestamp()


def api(path):
    return json.loads(subprocess.run(["gh", "api", path], check=True, capture_output=True, text=True).stdout)


def green_runs(repo):
    runs = api(f"repos/{repo}/actions/workflows/image.yml/runs?branch=main&status=success&per_page={RUNS}")
    out = []
    for run in runs.get("workflow_runs", []):
        if out and run["head_sha"] != out[0][0]:
            break
        jobs = api(f"repos/{repo}/actions/runs/{run['id']}/jobs").get("jobs", [])
        decided = next((j["completed_at"] for j in jobs if j["name"] == "changes" and j.get("completed_at")),
                       run["run_started_at"])
        out.append((run["head_sha"], epoch(decided)))
    return out


def selftest():
    a, b, c = "a" * 40, "b" * 40, "c" * 40
    now = 100_000
    assert wait_seconds(a, [], now, 3600) == 0, "no green run: build"
    assert wait_seconds(a, [(a, now - 10)], now, 3600) == 0, "newest image carries this sha: no wait (caller skips)"
    assert wait_seconds(a, [(b, now - 3601)], now, 3600) == 0, "built over an hour ago: build now"
    assert wait_seconds(a, [(b, now - 600)], now, 3600) == 3000, "built 10 min ago: wait the rest of the hour"
    assert wait_seconds(a, [(b, now - 60), (b, now - 1800), (c, now - 5000)], now, 3600) == 1800, \
        "the oldest run of the newest built commit counts, not a later no-op run"
    assert wait_seconds(a, [(b, now - 600)], now, 0) == 0, "gap 0 (ci-full's request): no wait"
    assert epoch("2026-10-23T09:08:40Z") == 1792746520.0
    print("image-gate selftest: ok")


def main(argv):
    if argv == ["--selftest"]:
        return selftest()
    if len(argv) != 3 or not REPO.match(argv[0]) or not SHA.match(argv[1]) or not argv[2].isdigit():
        sys.exit("usage: image-gate.py <owner/repo> <40-hex sha> <gap-seconds> | --selftest")
    repo, sha, gap = argv[0], argv[1], int(argv[2])
    # Re-read after a wait: a dispatch may have built a newer commit meanwhile. Bounded: each
    # round waits until the newest build is an hour old, so a third round finds nothing to wait.
    for _ in range(3):
        runs = green_runs(repo)
        wait = wait_seconds(sha, runs, time.time(), gap)
        if wait == 0:
            break
        print(f"newest image {runs[0][0][:7]} is younger than {gap} s: waiting {wait} s", file=sys.stderr)
        time.sleep(wait)
    print(runs[0][0] if runs else "")


if __name__ == "__main__":
    main(sys.argv[1:])
