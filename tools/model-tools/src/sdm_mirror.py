"""Every Smart Data Model converted ahead of time: the mirror the Portal reads (T-6353).

`import-sdm` fetches a model from GitHub and converts it on every click, 2 to 5 seconds. This
script does that once for the whole catalogue and writes what the Portal then serves from disk:

    index.json                          the catalogue as `GET /tools/sdm-catalog` answers it, each
                                        model with its upstream commit, and `failed`: the models
                                        that did not convert, each with its finding
    <subject>/<model>/linkml.yaml       the LinkML `import-sdm` would answer
    <subject>/<model>/source.json       the upstream commit and path (DM-08)

A model that does not convert or compile is listed in `failed` and has no directory: it is never
offered half broken. A run past `--max-failed` percent writes nothing, so a GitHub outage leaves
yesterday's mirror in place.
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
from collections.abc import Callable
from concurrent.futures import ProcessPoolExecutor
from datetime import UTC, datetime
from functools import cache
from pathlib import Path
from typing import Any

import yaml

from import_sdm import _composed, _segment, convert, fetch, split_identifier
from service import artifacts, fetch_catalogue

INDEX = "index.json"
GIT_BASE = "https://github.com/smart-data-models/"
#: One finding of a model is enough to read; a compile can repeat one message per generator.
MAX_FINDING = 500


def convert_one(model: str, read: Callable[[str], dict[str, Any]]) -> dict[str, Any]:
    """One model as the mirror keeps it: `linkml`, `commit`, `attributes`, or `finding` alone."""
    try:
        fetched = read(model)
        document = convert(
            model,
            fetched["schema"],
            fetched["context"],
            fetched["example"],
            fetched["provenance"],
            fetched.get("commons"),
        )
        source = yaml.safe_dump(document, sort_keys=False, allow_unicode=True)
        rendered = artifacts(source, linkml=source, example=fetched["example"])
    except Exception as err:  # noqa: BLE001 - one model failing is that model's finding
        return {"model": model, "finding": str(err)[:MAX_FINDING]}
    errors = rendered.get("errors") or []
    if errors:
        return {"model": model, "finding": "; ".join(str(e) for e in errors)[:MAX_FINDING]}
    properties = _composed(fetched["schema"], "properties")
    return {
        "model": model,
        "linkml": source,
        "commit": fetched["provenance"]["commit"],
        "path": fetched["provenance"]["path"],
        "attributes": sorted(properties) if isinstance(properties, dict) else [],
    }


@cache
def _commit_once(repository: str, ref: str = "master") -> str:
    """The branch head by git, not the REST API: 60 calls an hour per address outlast no run."""
    answer = subprocess.run(
        ["git", "ls-remote", f"{GIT_BASE}{_segment(repository, 'repository')}.git", f"refs/heads/{_segment(ref, 'ref')}"],
        capture_output=True,
        text=True,
        timeout=60,
        check=True,
    )
    return answer.stdout.split()[0]


def _read_upstream(model: str) -> dict[str, Any]:
    return fetch(model, commit_of=_commit_once)


def _convert_upstream(model: str) -> dict[str, Any]:
    return convert_one(model, _read_upstream)


def write(out: Path, subjects: list[dict[str, Any]], results: list[dict[str, Any]], now: str) -> None:
    """Replaces the mirror under `out` with `results`; the rest of `out` stays (.git, README)."""
    by_id = {result["model"]: result for result in results}
    for gone in out.glob("dataModel.*"):
        shutil.rmtree(gone)
    listed = []
    for subject in subjects:
        models = []
        for model in subject["models"]:
            result = by_id[model["id"]]
            if "finding" in result:
                continue
            models.append({**model, "attributes": result["attributes"], "commit": result["commit"]})
            folder = out.joinpath(*split_identifier(model["id"]))
            folder.mkdir(parents=True)
            (folder / "linkml.yaml").write_text(result["linkml"], encoding="utf-8")
            kept = {"commit": result["commit"], "path": result["path"]}
            (folder / "source.json").write_text(json.dumps(kept, indent=2) + "\n", encoding="utf-8")
        listed.append({**subject, "models": models})
    failed = [{"id": r["model"], "finding": r["finding"]} for r in results if "finding" in r]
    index = {"subjects": listed, "failed": sorted(failed, key=lambda one: one["id"])}
    path = out / INDEX
    # The time moves only with the content, so a night with no upstream change is no commit.
    before = json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}
    refreshed = before.get("refreshedAt", now) if {**before, "refreshedAt": None} == {**index, "refreshedAt": None} else now
    body = json.dumps({"refreshedAt": refreshed, **index}, indent=1, ensure_ascii=False)
    path.write_text(body + "\n", encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", required=True, type=Path, help="the mirror's working copy")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--max-failed", type=float, default=10, help="percent of the catalogue")
    parser.add_argument("--only", action="append", default=[], help="catalogue identifiers, for a trial")
    args = parser.parse_args(argv)

    subjects = fetch_catalogue()
    if args.only:
        subjects = [
            {**s, "models": [m for m in s["models"] if m["id"] in args.only]} for s in subjects
        ]
        subjects = [s for s in subjects if s["models"]]
    models = [model["id"] for subject in subjects for model in subject["models"]]
    with ProcessPoolExecutor(args.jobs) as pool:
        results = list(pool.map(_convert_upstream, models, chunksize=4))
    failed = [result for result in results if "finding" in result]
    print(f"{len(models) - len(failed)} converted, {len(failed)} failed", file=sys.stderr)
    if not models or len(failed) * 100 > len(models) * args.max_failed:
        print("too many failures: the mirror stays as it is", file=sys.stderr)
        for result in failed[:20]:
            print(f"{result['model']}: {result['finding']}", file=sys.stderr)
        return 2
    write(args.out, subjects, results, datetime.now(UTC).strftime("%Y-%m-%dT%H:%M:%SZ"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
