"""The golden test runner that proves the compiler right (T-0174, DM-39, DM-52, TS-16).

A Mapping is written once and executed twice: the reference engine reads the
`TransformationSpecification` itself, and Bento runs the Bloblang this repository compiled
from it. Nothing forces those two to agree except this runner. It takes every golden test of
a Mapping (`spec.tests[]`, DM-39), pushes the input through both engines, and fails on any
difference, which is what turns "the compiler looks right" into a fact CI can check.

Values are compared decoded, not as bytes. JSON has one number type and the two runtimes
print it differently: Python writes `250.0` where Bento writes `250`, and treating that as a
difference would fail every mapping that converts a unit while catching no real defect.

Bento is executed as `bento test` on a generated unit-test file, which is the form DM-39 and
TS-16 name. It is found as `$BENTO`, as `bento` on PATH, or as the pinned image through
docker; with none of the three the Bloblang engine does not run, and the runner says so
rather than reporting a parity it never checked.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

import yaml
from linkml_map.transformer.object_transformer import ObjectTransformer
from linkml_runtime import SchemaView

from common import ModelError
from compile_bloblang import compile_bloblang

#: The Bento the deployment pins, used when there is no local binary. Kept equal to
#: `components/pipeline-runner/images.yaml` in joinedcontext-deployment, digest and all: parity
#: proven against a different Bento than the one the cluster runs is parity proven against
#: nothing, and a tag is not a version.
BENTO_IMAGE = (
    "ghcr.io/warpstreamlabs/bento"
    "@sha256:656c55de3f8deddd4ee743f3c76f3b497e67324e940f2bc1769693cd8b906364"  # 1.21.1
)


class ParityError(Exception):
    """A golden test the two engines answered differently."""


def reference(transformation: Any, source_schema: str, source_class: str, entity: dict) -> dict:
    """One entity through the LinkML-Map reference engine, the specification's own semantics."""
    transformer = ObjectTransformer()
    transformer.source_schemaview = SchemaView(source_schema)
    transformer.create_transformer_specification(json.loads(json.dumps(transformation)))
    return transformer.map_object(entity, source_type=source_class)


def bento_command() -> list[str] | None:
    """How to run Bento here, or None where it cannot be run at all."""
    binary = os.environ.get("BENTO") or shutil.which("bento")
    if binary:
        return [binary]
    if shutil.which("docker"):
        return ["docker", "run", "--rm", "-i", "-v", "{mount}:/w", BENTO_IMAGE]
    return None


def bento(mapping: str, cases: list[tuple[dict, dict]]) -> None:
    """Every golden case through `bento test`, raising on the first that does not match.

    The generated file is Bento's own unit-test form: the compiled mapping as the single
    processor, each input as an `input_batch` and each expectation as `json_equals`. Bento
    decides whether they match, so this is Bento's comparison and not ours.
    """
    command = bento_command()
    if command is None:
        raise ParityError("no bento binary and no docker, so the compiled mapping never ran")
    with tempfile.TemporaryDirectory() as directory:
        work = Path(directory)
        config = {"input": {"stdin": {}},
                  "pipeline": {"processors": [{"mapping": mapping}]},
                  "output": {"drop": {}}}
        tests = [
            {
                "name": f"golden {index}",
                "target_processors": "/pipeline/processors",
                "input_batch": [{"json_content": source}],
                "output_batches": [[{"json_equals": expected}]],
            }
            for index, (source, expected) in enumerate(cases)
        ]
        (work / "mapping.yaml").write_text(yaml.safe_dump(config, default_flow_style=False))
        (work / "mapping_bento_test.yaml").write_text(yaml.safe_dump({"tests": tests}))
        # `mkdtemp` is 0700 and the Bento image runs as its own unprivileged user, so a
        # mounted directory it cannot traverse fails before the mapping is ever parsed.
        work.chmod(0o755)
        for entry in work.iterdir():
            entry.chmod(0o644)

        if command[0] == "docker":
            argv = [part.format(mount=str(work)) for part in command] + ["test", "/w/mapping.yaml"]
        else:
            argv = command + ["test", str(work / "mapping.yaml")]
        result = subprocess.run(argv, capture_output=True, text=True)
        if result.returncode != 0:
            raise ParityError(f"bento test failed:\n{result.stdout}{result.stderr}")


def check(mapping: dict[str, Any], base: Path, source_schema: str) -> dict[str, Any]:
    """Run every golden test of one Mapping through both engines (DM-39).

    `mapping` is the manifest or its `spec`; `base` is the directory the `tests[]` paths are
    relative to; `source_schema` is the LinkML of the model `spec.source` names. Resolving
    that reference is the caller's job, because it means reading another manifest out of the
    repository and Model Tools reads no repository (DM-18). Returns what ran, so a caller can
    tell a passing run from a run where the Bloblang engine was never available.
    """
    spec = mapping.get("spec", mapping)
    transformation = spec.get("transformation")
    if not transformation:
        raise ModelError("the Mapping carries no spec.transformation")
    cases = spec.get("tests") or []
    if not cases:
        raise ModelError("the Mapping carries no golden test, and DM-39 requires at least one")
    schema_path = str(Path(source_schema).resolve())

    artifact = compile_bloblang(transformation, source=schema_path, native=spec.get("native") or ())
    pairs: list[tuple[dict, dict]] = []
    differences: list[str] = []

    for index, case in enumerate(cases):
        source = _read(base, case["input"])
        expected = _read(base, case["expect"])
        produced = reference(transformation, schema_path, artifact.source_class, source)
        if produced != expected:
            differences.append(
                f"golden test {index} ({case['input']}): the specification produces "
                f"{json.dumps(produced, sort_keys=True, default=str)}, and the committed "
                f"expectation is {json.dumps(expected, sort_keys=True)}"
            )
        pairs.append((source, expected))

    if differences:
        raise ParityError("\n".join(differences))

    engines = ["reference"]
    if bento_command() is not None:
        bento(artifact.mapping, pairs)
        engines.append("bloblang")
    return {"cases": len(pairs), "engines": engines, "mapping": artifact.mapping}


def _resolve(base: Path, reference_path: str) -> Path:
    path = (base / reference_path).resolve()
    if not path.is_file():
        raise ModelError(f"{reference_path} does not exist beside the Mapping")
    return path


def _read(base: Path, reference_path: str) -> Any:
    return json.loads(_resolve(base, reference_path).read_text())


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("mapping", nargs="+", help="kind: Mapping manifests to check")
    parser.add_argument(
        "--source-schema", required=True,
        help="LinkML of the model spec.source names, resolved by the caller",
    )
    arguments = parser.parse_args(argv)

    failed = False
    for name in arguments.mapping:
        path = Path(name).resolve()
        try:
            outcome = check(yaml.safe_load(path.read_text()), path.parent, arguments.source_schema)
        except (ModelError, ParityError) as error:
            print(f"{path.name}: FAILED\n{error}", file=sys.stderr)
            failed = True
            continue
        engines = ", ".join(outcome["engines"])
        print(f"{path.name}: {outcome['cases']} golden test(s) agree across {engines}")
        if "bloblang" not in outcome["engines"]:
            print(f"{path.name}: the compiled Bloblang did not run, so DM-39 is not satisfied",
                  file=sys.stderr)
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
