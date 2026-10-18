#!/usr/bin/env python3
"""The context-gateway holds no format code (ADR-N-051, T-4476): a ratchet that only shrinks.

ci/gateway-formats-baseline.txt lists what is still there, each row with the task that moves it:

    crate <name> | <task>              a format crate crates/context-gateway/Cargo.toml depends on
    file <path> <lines> | <task>       a format module and its line count (`wc -l`)

Red when:
  - the gateway depends on a format crate the baseline does not list;
  - a file under translators/, or a format module ADR-N-051 names, exists unlisted;
  - a listed file has grown by more than 20 lines over its baseline count;
  - a row names a crate or a file that is gone (stale: the move deletes its row in the same commit);
  - a row has no task.

  scripts/ci/check-gateway-formats.py
  scripts/ci/check-gateway-formats.py --selftest
"""

from __future__ import annotations

import pathlib
import sys
import tempfile
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[2]
BASELINE = "ci/gateway-formats-baseline.txt"
GATEWAY = "crates/context-gateway"
FORMAT_CRATES = {"zip", "parquet", "csv", "rust_xlsxwriter", "calamine", "geojson", "geo-types", "wkb"}
# The modules ADR-N-051 section 3 moves out besides translators/.
FORMAT_MODULES = [
    "src/handlers/sta.rs",
    "src/handlers/files.rs",
    "src/handlers/reads.rs",
    "src/query.rs",
    "src/mcp/tools.rs",
    "src/mcp/units.rs",
    "src/handlers/endpoint_surface.rs",
]
GROWTH = 20


def is_format_crate(name: str) -> bool:
    return name in FORMAT_CRATES or name.startswith("arrow")


def watched(root: pathlib.Path) -> dict[str, int]:
    """Every format file of the gateway that exists, with its line count."""
    gateway = root / GATEWAY
    paths = sorted((gateway / "src" / "translators").glob("**/*.rs"))
    paths += [gateway / m for m in FORMAT_MODULES if (gateway / m).is_file()]
    return {str(p.relative_to(root)): len(p.read_text(encoding="utf-8").splitlines()) for p in paths}


def check(root: pathlib.Path) -> list[str]:
    deps = tomllib.loads((root / GATEWAY / "Cargo.toml").read_text(encoding="utf-8")).get("dependencies", {})
    crates = {name for name in deps if is_format_crate(name)}
    files = watched(root)
    problems, listed_crates, listed_files = [], set(), {}
    for number, line in enumerate((root / BASELINE).read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip() or line.startswith("#"):
            continue
        row, _, task = line.partition(" | ")
        parts = row.split()
        if not task.strip().startswith("T-"):
            problems.append(f"{BASELINE}:{number}: no moving task: {line}")
        if len(parts) == 2 and parts[0] == "crate":
            listed_crates.add(parts[1])
        elif len(parts) == 3 and parts[0] == "file" and parts[2].isdigit():
            listed_files[parts[1]] = int(parts[2])
        else:
            problems.append(f"{BASELINE}:{number}: not `crate <name> | T-x` or `file <path> <lines> | T-x`: {line}")
    problems += [f"{GATEWAY} depends on format crate {c}, which ADR-N-051 keeps out" for c in sorted(crates - listed_crates)]
    problems += [f"stale row: crate {c} is no longer a dependency; delete its row" for c in sorted(listed_crates - crates)]
    for path, lines in files.items():
        if path not in listed_files:
            problems.append(f"{path}: format code in the gateway ({lines} lines), which ADR-N-051 keeps out")
        elif lines > listed_files[path] + GROWTH:
            problems.append(f"{path}: {lines} lines, over its baseline {listed_files[path]} + {GROWTH}")
    problems += [f"stale row: {p} is gone; delete its row" for p in sorted(set(listed_files) - set(files))]
    return problems


def selftest() -> None:
    with tempfile.TemporaryDirectory() as d:
        root = pathlib.Path(d)
        src = root / GATEWAY / "src"
        (src / "translators").mkdir(parents=True)
        (root / "ci").mkdir()
        cargo = root / GATEWAY / "Cargo.toml"
        cargo.write_text('[package]\nname = "context-gateway"\n[dependencies]\nzip = "2"\nserde = "1"\n')
        (src / "translators" / "sta.rs").write_text("x\n" * 10)
        (src / "query.rs").write_text("x\n" * 5)
        base = root / BASELINE
        ok = f"crate zip | T-4478\nfile {GATEWAY}/src/translators/sta.rs 10 | T-4463\nfile {GATEWAY}/src/query.rs 5 | T-4479\n"
        base.write_text(ok)
        assert check(root) == [], check(root)
        # a new format crate
        cargo.write_text(cargo.read_text() + 'parquet = "55"\narrow-array = "55"\n')
        assert [p for p in check(root) if "parquet" in p or "arrow" in p] and len(check(root)) == 2, check(root)
        cargo.write_text('[package]\nname = "context-gateway"\n[dependencies]\nzip = "2"\nserde = "1"\n')
        # a new translators file, and a new format module
        (src / "translators" / "csv.rs").write_text("x\n")
        assert check(root) == [f"{GATEWAY}/src/translators/csv.rs: format code in the gateway (1 lines), which ADR-N-051 keeps out"]
        (src / "translators" / "csv.rs").unlink()
        # a grown file: 20 lines more is tolerated, 21 is not
        (src / "query.rs").write_text("x\n" * 25)
        assert check(root) == []
        (src / "query.rs").write_text("x\n" * 26)
        assert check(root) == [f"{GATEWAY}/src/query.rs: 26 lines, over its baseline 5 + 20"]
        # a deleted file with its row deleted passes; with the row kept, the row is stale
        (src / "query.rs").unlink()
        assert check(root) == [f"stale row: {GATEWAY}/src/query.rs is gone; delete its row"]
        base.write_text(ok.replace(f"file {GATEWAY}/src/query.rs 5 | T-4479\n", ""))
        assert check(root) == []
        # a crate dropped with its row kept is stale; a row without a task is red
        cargo.write_text('[package]\nname = "context-gateway"\n[dependencies]\nserde = "1"\n')
        assert check(root) == ["stale row: crate zip is no longer a dependency; delete its row"]
        base.write_text(f"file {GATEWAY}/src/translators/sta.rs 10 | later\n")
        assert any("no moving task" in p for p in check(root)), check(root)
    print("check-gateway-formats selftest: ok")


def main(argv: list[str]) -> int:
    if argv[1:] == ["--selftest"]:
        selftest()
        return 0
    problems = check(ROOT)
    for p in problems:
        print(p)
    print(f"check-gateway-formats: {len(problems)} problem(s)" if problems else "check-gateway-formats: OK")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
