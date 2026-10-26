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

When `sha` is that newest built commit but no run built it on today's UTC day, the line reads
`<sha> refresh` (T-5485): the caller builds the images again, so the Dockerfile's APT_REFRESH
layer takes the Debian security fixes released since. Without it a quiet main kept serving the
day-old packages (libbrotli1 CVE-2026-111497 on 2026-10-24, main unmerged since the day before).
"""
import json
import re
import subprocess
import sys
import time
from datetime import datetime

SHA = re.compile(r"^[0-9a-f]{40}$")
REPO = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
# Green runs read per call: a day of hourly no-op runs of one quiet commit, so its newest build
# is still in view.
RUNS = 30


def wait_seconds(sha, runs, now, gap):
    """Seconds to wait before building `sha`. `runs`: (head_sha, decided_epoch, built) of green runs
    on main, newest first."""
    if not runs or runs[0][0] == sha:
        return 0
    first = runs[0][1]
    for s, t, _ in runs:
        if s != runs[0][0]:
            break
        first = min(first, t)
    return max(0, int(first + gap - now))


def refresh_due(sha, runs, now):
    """`sha` is the newest built commit and none of its runs in view built it on `now`'s UTC day.
    `runs`: (head_sha, decided_epoch, built) of green runs on main, newest first."""
    if not runs or runs[0][0] != sha:
        return False
    day = time.gmtime(now)[:3]
    return not any(s == sha and built and time.gmtime(t)[:3] == day for s, t, built in runs)


def epoch(stamp):
    return datetime.fromisoformat(stamp).timestamp()


# Pauses between `gh api` attempts: one GitHub hiccup failed the whole gate (ship run 40377654855,
# T-5812), so a call is tried len(RETRY_PAUSES) + 1 times before the gate gives up.
RETRY_PAUSES = (5, 20)


def api(path, run=subprocess.run, sleep=time.sleep):
    for pause in (*RETRY_PAUSES, None):
        done = run(["gh", "api", path], capture_output=True, text=True)
        if done.returncode == 0:
            return json.loads(done.stdout)
        error = done.stderr.strip() or f"exit {done.returncode}"
        if pause is None:
            sys.exit(f"image-gate: gh api {path} failed: {error}")
        print(f"image-gate: gh api {path} failed ({error}), retrying in {pause} s", file=sys.stderr)
        sleep(pause)


def green_runs(repo):
    runs = api(f"repos/{repo}/actions/workflows/image.yml/runs?branch=main&status=success&per_page={RUNS}")
    out = []
    for run in runs.get("workflow_runs", []):
        if out and run["head_sha"] != out[0][0]:
            break
        jobs = api(f"repos/{repo}/actions/runs/{run['id']}/jobs").get("jobs", [])
        decided = next((j["completed_at"] for j in jobs if j["name"] == "changes" and j.get("completed_at")),
                       run["run_started_at"])
        built = any(j["name"] == "build" and j.get("conclusion") == "success" for j in jobs)
        out.append((run["head_sha"], epoch(decided), built))
    return out


def selftest():
    a, b, c = "a" * 40, "b" * 40, "c" * 40
    now = 100_000
    assert wait_seconds(a, [], now, 3600) == 0, "no green run: build"
    assert wait_seconds(a, [(a, now - 10, True)], now, 3600) == 0, "newest image carries this sha: no wait (caller skips)"
    assert wait_seconds(a, [(b, now - 3601, True)], now, 3600) == 0, "built over an hour ago: build now"
    assert wait_seconds(a, [(b, now - 600, True)], now, 3600) == 3000, "built 10 min ago: wait the rest of the hour"
    assert wait_seconds(a, [(b, now - 60, False), (b, now - 1800, False), (c, now - 5000, True)], now, 3600) == 1800, \
        "the oldest run of the newest built commit counts, not a later no-op run"
    assert wait_seconds(a, [(b, now - 600, True)], now, 0) == 0, "gap 0 (ci-full's request): no wait"
    noon = 1792843200  # 2026-10-24T12:00:00Z
    assert not refresh_due(a, [], noon), "no green run: the caller builds anyway"
    assert not refresh_due(a, [(b, noon - 60, True)], noon), "another commit is newest: the caller builds anyway"
    assert not refresh_due(a, [(a, noon - 60, False), (a, noon - 3600, True)], noon), "built today: skip"
    assert refresh_due(a, [(a, noon - 60, False), (a, noon - 13 * 3600, True)], noon), "built yesterday: rebuild"
    assert refresh_due(a, [(a, noon - 60, False)], noon), "its build out of view: rebuild once"
    assert not refresh_due(a, [(a, noon - 60, True), (b, noon - 13 * 3600, True)], noon), \
        "a refresh build today counts, not the older commit's"
    assert epoch("2026-10-23T09:08:40Z") == 1792746520.0
    answers = iter([(1, "", "HTTP 502: Bad Gateway"), (0, '{"ok": 1}', "")])
    fake = lambda *_, **__: subprocess.CompletedProcess([], *next(answers))
    pauses = []
    assert api("x", fake, pauses.append) == {"ok": 1}, "a failed call is retried"
    assert pauses == [RETRY_PAUSES[0]], "after the first pause"
    fake = lambda *_, **__: subprocess.CompletedProcess([], 1, "", "HTTP 403: rate limit")
    try:
        api("x", fake, pauses.append)
        raise AssertionError("a call failing every attempt must stop the gate")
    except SystemExit as stop:
        assert "rate limit" in str(stop.code), "the gate names gh's error"
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
    if not runs:
        print()
    else:
        print(runs[0][0] + (" refresh" if refresh_due(sha, runs, time.time()) else ""))


if __name__ == "__main__":
    main(sys.argv[1:])
