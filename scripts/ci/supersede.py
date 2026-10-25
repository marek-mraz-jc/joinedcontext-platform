#!/usr/bin/env python3
"""Cancel the main ci runs this run supersedes (T-5659).

    supersede.py              (in ci.yml on a push to main; GITHUB_REPOSITORY, GITHUB_SHA, GITHUB_RUN_ID)
    supersede.py --selftest

Every main commit gets its own ci run (T-5022, T-4696). When the account's job slots are full the
runs pile up and main goes hours without a verdict, yet tasks/ci closes a task once a later green
main run contains its commit, so the newest run is the only one needed. This run cancels every
other ci run on main that
- is still queued or in progress, with no job running right now (a job already running finishes),
- is a push run of this workflow on main (never a branch, a pull request or another workflow),
- has a head commit that is a strict ancestor of this run's commit (GitHub's compare API says
  "ahead"), so this run checks everything that run would have,
- and has no image.yml run on its commit: pin-images pins only a built commit whose ci is green,
  so the commit image.yml took keeps its verdict.
ponytail: a job can start between the check and the cancel; it is then cancelled with its run, and
the newer run repeats its work. Needs GH_TOKEN with actions: write on this repository.
A copy lives in joinedcontext-platform and joinedcontext-portal; keep the two equal.
"""
import json
import os
import re
import subprocess
import sys

SHA = re.compile(r"^[0-9a-f]{40}$")
REPO = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
OPEN = {"queued", "in_progress", "waiting", "pending", "requested"}


def superseded(own_id, own_sha, runs, is_ancestor, busy, built):
    """The ids of `runs` ({id, head_branch, event, head_sha, status}) this run cancels.

    is_ancestor(sha) is true when sha is a strict ancestor of own_sha; busy(id) when a job of that
    run is running; built(sha) when image.yml ran on that commit."""
    out = []
    for r in runs:
        if r["id"] == own_id or r["head_branch"] != "main" or r["event"] != "push":
            continue
        if r["status"] not in OPEN or r["head_sha"] == own_sha:
            continue
        if is_ancestor(r["head_sha"]) and not busy(r["id"]) and not built(r["head_sha"]):
            out.append(r["id"])
    return out


def gh(*args):
    result = subprocess.run(["gh", "api", *args], capture_output=True, text=True, check=False)
    if result.returncode != 0:
        sys.exit(f"supersede: gh api {' '.join(args)} failed: {result.stderr.strip()} "
                 "(the job needs permissions actions: write and contents: read)")
    return result.stdout


def main():
    repo, own_sha, own_id = (os.environ.get(k, "") for k in ("GITHUB_REPOSITORY", "GITHUB_SHA", "GITHUB_RUN_ID"))
    if not (REPO.match(repo) and SHA.match(own_sha) and own_id.isdigit()):
        sys.exit("supersede: runs inside ci.yml only: GITHUB_REPOSITORY owner/name, GITHUB_SHA a 40-hex "
                 "commit and GITHUB_RUN_ID a number")
    own_id = int(own_id)
    runs = json.loads(gh(f"repos/{repo}/actions/workflows/ci.yml/runs?branch=main&event=push&per_page=100",
                         "--jq", "[.workflow_runs[] | {id, head_branch, event, head_sha, status}]"))

    def is_ancestor(sha):
        return gh(f"repos/{repo}/compare/{sha}...{own_sha}", "--jq", ".status").strip() == "ahead"

    def busy(run_id):
        return gh(f"repos/{repo}/actions/runs/{run_id}/jobs?per_page=100",
                  "--jq", '[.jobs[] | select(.status == "in_progress")] | length').strip() != "0"

    def built(sha):
        return gh(f"repos/{repo}/actions/workflows/image.yml/runs?head_sha={sha}&per_page=1",
                  "--jq", ".total_count").strip() != "0"

    gone = superseded(own_id, own_sha, runs, is_ancestor, busy, built)
    for run_id in gone:
        print(f"cancel run {run_id}: its commit is in {own_sha[:7]} and none of its jobs is running")
        gh("-X", "POST", f"repos/{repo}/actions/runs/{run_id}/cancel")
    print(f"supersede: {len(gone)} older main run(s) cancelled")


def selftest():
    a, b, c, d, own = ("a" * 40, "b" * 40, "c" * 40, "d" * 40, "f" * 40)

    def run(i, sha, status="queued", branch="main", event="push"):
        return {"id": i, "head_branch": branch, "event": event, "head_sha": sha, "status": status}

    runs = [
        run(9, own, "in_progress"),            # this run
        run(8, "e" * 40),                       # newer than this run: not an ancestor
        run(7, a),                              # queued ancestor: cancelled
        run(6, b, "in_progress"),               # a job of it runs: kept
        run(5, c, "in_progress"),               # in progress, its jobs only queued: cancelled
        run(4, a, branch="agent/w/T-1"),        # a branch run: kept
        run(3, a, event="pull_request"),        # a pull request run: kept
        run(2, d),                              # image.yml built it: kept
        run(1, a, "completed"),                 # finished: nothing to cancel
        run(10, own),                           # a rerun of this commit: kept
    ]
    ancestors, running, images = {a, b, c, d}, {6}, {d}
    got = superseded(9, own, runs, ancestors.__contains__, running.__contains__, images.__contains__)
    assert got == [7, 5], f"only the idle ancestor main push runs go, got {got}"
    assert superseded(9, own, runs, lambda s: False, running.__contains__, images.__contains__) == [], \
        "no ancestor, nothing cancelled"
    assert superseded(9, own, [run(9, own)], ancestors.__contains__, running.__contains__, images.__contains__) == [], \
        "a run never cancels itself"
    print("supersede selftest: ok")


if __name__ == "__main__":
    if sys.argv[1:] == ["--selftest"]:
        selftest()
    elif sys.argv[1:]:
        sys.exit(__doc__)
    else:
        main()
