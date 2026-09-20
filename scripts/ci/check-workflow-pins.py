#!/usr/bin/env python3
"""Every action a lane runs is a commit, and only a job that publishes may publish (T-0851).

A tag is a reference somebody else owns: whoever moves one runs code in these workflows, and
`image.yml` carries `packages: write` and `id-token: write` — the two permissions that push an
image under the project's name and mint the identity that signs it. A workflow-level grant hands
both to every job in the file, including one added later for something unrelated.

Line-based on purpose: the runner has no YAML library installed by default, and both rules are
decidable from the text. Run it from anywhere: `python3 scripts/ci/check-workflow-pins.py`.
"""

import re
import sys
from pathlib import Path

WORKFLOWS = Path(__file__).resolve().parents[2] / ".github/workflows"
USES = re.compile(r"^\s*(?:- )?uses:\s*(\S+)")
COMMIT = re.compile(r"@[0-9a-f]{40}$")
PUBLISHES = ("packages: write", "id-token: write")


def problems(path: Path):
    lines = path.read_text().splitlines()
    for number, line in enumerate(lines, 1):
        used = USES.match(line)
        # A local `./…` call is this repository at the commit already checked out; there is no
        # third-party tag in it to move.
        if used and not used.group(1).startswith("./") and not COMMIT.search(used.group(1)):
            yield f"{path.name}:{number}: {used.group(1)} is a tag, not a commit"
        if not line.startswith("permissions:"):
            continue
        block = [line]
        for follow in lines[number:]:
            if follow.startswith((" ", "\t", "#")) or not follow.strip():
                block.append(follow)
            else:
                break
        for grant in PUBLISHES:
            if grant in "\n".join(block):
                yield (
                    f"{path.name}:{number}: `{grant}` is granted to every job in the file; "
                    "grant it on the job that publishes"
                )


def main() -> int:
    found = [problem for path in sorted(WORKFLOWS.glob("*.yml")) for problem in problems(path)]
    for problem in found:
        print(problem, file=sys.stderr)
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
