"""Fixtures shared by the Model Tools suites."""

from __future__ import annotations

import json
import subprocess
import tempfile
from pathlib import Path

import pytest

from test_mapping_parity import bento_command

FIXTURES = Path(__file__).parent / "fixtures"


@pytest.fixture
def senzor() -> str:
    """A model with every NGSI-LD kind the generators treat specially."""
    return str(FIXTURES / "senzor.linkml.yaml")


@pytest.fixture
def squatted() -> str:
    """A model minting a term under a namespace the organisation does not own."""
    return str(FIXTURES / "squatted.linkml.yaml")


@pytest.fixture
def sdm_schema() -> dict:
    return json.loads((FIXTURES / "sdm-airqualityobserved.schema.json").read_text())


@pytest.fixture
def sdm_commons() -> dict:
    """The shared commons document every catalogue schema `$ref`s."""
    return json.loads((FIXTURES / "sdm-common-schema.json").read_text())


@pytest.fixture
def sdm_properties(sdm_schema, sdm_commons) -> dict:
    """The attributes the fixture schema declares, read the way the importer reads them.

    A catalogue schema is an `allOf` of the shared commons and one inline branch, so the
    attributes are never at the top level, and the commons branch is a `$ref`. Tests compose
    them the same way the importer does, rather than each knowing the layout.
    """
    from import_sdm import _composed, resolve

    return _composed(resolve(sdm_schema, sdm_commons), "properties")


@pytest.fixture
def sdm_context() -> dict:
    return json.loads((FIXTURES / "sdm-airqualityobserved.context.jsonld").read_text())


@pytest.fixture
def sdm_provenance() -> dict:
    return {
        "repository": "https://github.com/smart-data-models/dataModel.Environment",
        "path": "AirQualityObserved/schema.json",
        "commit": "8c4f2b1a9e6d0f3c5b7a1d2e4f6a8b0c2d4e6f80",
    }


@pytest.fixture
def mapping_source() -> str:
    """The upstream air quality model a Mapping reads."""
    return str(FIXTURES / "mapping-source.linkml.yaml")


@pytest.fixture
def mapping_manifest() -> Path:
    """The golden `kind: Mapping`, beside the input and expected examples it names."""
    return FIXTURES / "mapping" / "airquality.yaml"


@pytest.fixture
def transformation(mapping_manifest) -> dict:
    """The `TransformationSpecification` of the golden Mapping, as the manifest carries it."""
    import yaml

    return yaml.safe_load(mapping_manifest.read_text())["spec"]["transformation"]


#: Bento is how a compiled mapping is proven to be Bloblang at all: nothing else here parses
#: the language. It is not always present, so the tests that need it say so and skip rather
#: than passing without having run anything.
requires_bento = pytest.mark.skipif(
    bento_command() is None, reason="no bento binary and no docker to run the pinned image in"
)


def bloblang(mapping: str, document: dict) -> dict:
    """One document through a compiled mapping, as `bento blobl` executes it.

    Raises `RuntimeError` where the mapping fails, which is how a compiled `throw()` is
    asserted: a refusal the specification also makes is a result, not an absence of one.
    """
    command = bento_command()
    assert command is not None, "bloblang() is only reachable behind requires_bento"
    with tempfile.TemporaryDirectory() as directory:
        work = Path(directory)
        (work / "mapping.blobl").write_text(mapping)
        work.chmod(0o755)
        (work / "mapping.blobl").chmod(0o644)
        if command[0] == "docker":
            argv = [part.format(mount=str(work)) for part in command] + ["blobl", "-f",
                                                                         "/w/mapping.blobl"]
        else:
            argv = command + ["blobl", "-f", str(work / "mapping.blobl")]
        result = subprocess.run(argv, input=json.dumps(document), capture_output=True, text=True)
    if result.returncode != 0 or not result.stdout.strip():
        raise RuntimeError(f"{result.stdout}{result.stderr}".strip())
    return json.loads(result.stdout)
