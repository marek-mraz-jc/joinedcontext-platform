#!/usr/bin/env python3
"""Every command the README tells a newcomer to run is a command that works (T-2144, TS-19).

A README is the first thing a stranger and a security reviewer read, and the commands in it
are the first thing they type. This extracts the fenced `bash` blocks under the numbered
**Build** and **Test** headings and runs them, so a command that rots — a renamed crate, a
moved script, a flag that no longer exists — fails a lane instead of a person's first hour.

Only those two sections run. **Run locally** starts a server and **Security** is prose; a
command there is checked by eye, which is what the section is for.

    check-readme-commands.py [readme] [--list] [--selftest]
"""

from __future__ import annotations

import argparse
import pathlib
import re
import subprocess
import sys

SECTIONS = ("Build", "Test")
HEADING = re.compile(r"^##\s+\d*\.?\s*(.+?)\s*$")
FENCE = re.compile(r"^```(\w*)\s*$")


def commands_of(text: str) -> list[str]:
    """Every line of every bash block under a Build or Test heading, joined by continuation."""
    found: list[str] = []
    section: str | None = None
    language: str | None = None
    block: list[str] = []
    for line in text.splitlines():
        heading = HEADING.match(line)
        if heading and language is None:
            section = heading.group(1)
            continue
        fence = FENCE.match(line)
        if fence is not None:
            if language is None:
                language = fence.group(1)
                block = []
            else:
                if language == "bash" and section in SECTIONS:
                    found.extend(joined(block))
                language = None
            continue
        if language is not None:
            block.append(line)
    return found


def joined(block: list[str]) -> list[str]:
    """One command per entry, with a trailing backslash continuing onto the next line."""
    commands: list[str] = []
    current: list[str] = []
    for line in block:
        piece = line.strip()
        if not piece or piece.startswith("#"):
            continue
        if piece.endswith("\\"):
            current.append(piece[:-1].rstrip())
            continue
        current.append(piece)
        commands.append(" ".join(current))
        current = []
    if current:
        commands.append(" ".join(current))
    return commands


SELFTEST = """# A README

## 1. Build

```bash
cargo build --locked
```

## 2. Test

```bash
# a comment is not a command
cargo fmt --all --check
cargo clippy --workspace \\
  -- -D warnings
```

```text
this fence is prose, not a command
```

## 3. Run locally

```bash
cargo run -p context-gateway
```
"""


def selftest() -> int:
    """The parser reads what it is meant to read, and leaves the rest alone.

    Without this, a parser that silently found nothing would make the lane pass for ever.
    """
    found = commands_of(SELFTEST)
    expected = [
        "cargo build --locked",
        "cargo fmt --all --check",
        "cargo clippy --workspace -- -D warnings",
    ]
    if found != expected:
        print(f"the parser read {found!r}, not {expected!r}", file=sys.stderr)
        return 1
    if commands_of("# A README with no sections\n") != []:
        print("the parser invented a command out of a README with none", file=sys.stderr)
        return 1
    print("check-readme-commands: selftest passed")
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("readme", nargs="?", default="README.md")
    parser.add_argument("--list", action="store_true", help="print the commands and run none")
    parser.add_argument("--selftest", action="store_true", help="check the parser itself")
    arguments = parser.parse_args(argv[1:])

    if arguments.selftest:
        return selftest()

    path = pathlib.Path(arguments.readme)
    commands = commands_of(path.read_text(encoding="utf-8"))
    if not commands:
        print(
            f"{path}: no bash block under a Build or Test heading; either the README lost its "
            "sections or this parser did",
            file=sys.stderr,
        )
        return 1

    if arguments.list:
        for command in commands:
            print(command)
        return 0

    failed = []
    for command in commands:
        print(f"$ {command}", flush=True)
        if subprocess.run(command, shell=True, cwd=path.parent or ".").returncode != 0:
            failed.append(command)
    if failed:
        print(
            f"\n{len(failed)} README command(s) do not work:",
            *failed,
            sep="\n  ",
            file=sys.stderr,
        )
        return 1
    print(f"\n{len(commands)} README command(s) ran")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
