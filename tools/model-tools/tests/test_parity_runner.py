"""The golden test runner (T-0174, DM-39, DM-52, TS-16).

The runner is the only thing that makes the compiler's correctness a fact rather than a
reading of the source. Every golden test of a Mapping goes through the LinkML-Map reference
engine, which is the specification's own semantics, and through Bento running the compiled
Bloblang, which is what the cluster executes. The tests here run it over the committed
fixtures, and then break each engine in turn to prove the runner actually fails when they
disagree: a green runner that cannot go red proves nothing.
"""

from __future__ import annotations

import json

import pytest
import yaml

import test_mapping_parity
from common import ModelError
from conftest import requires_bento
from test_mapping_parity import ParityError, check, main


@pytest.fixture
def manifest(mapping_manifest) -> dict:
    return yaml.safe_load(mapping_manifest.read_text())


def test_the_golden_mapping_agrees_across_both_engines(manifest, mapping_manifest,
                                                       mapping_source):
    outcome = check(manifest, mapping_manifest.parent, mapping_source)
    assert outcome["cases"] == 2
    assert "reference" in outcome["engines"]


@requires_bento
def test_the_bloblang_engine_really_runs(manifest, mapping_manifest, mapping_source):
    outcome = check(manifest, mapping_manifest.parent, mapping_source)
    # Without this the runner would report a parity nobody checked, which is the one failure
    # mode a golden test runner cannot have (DM-39).
    assert outcome["engines"] == ["reference", "bloblang"]


def test_a_wrong_expectation_fails_the_reference_engine(manifest, mapping_manifest,
                                                        mapping_source, tmp_path):
    for name in ("airquality.yaml", "good.input.json", "good.expect.json",
                 "poor.input.json", "poor.expect.json"):
        (tmp_path / name).write_text((mapping_manifest.parent / name).read_text())
    expectation = json.loads((tmp_path / "good.expect.json").read_text())
    expectation["band"] = "Z"
    (tmp_path / "good.expect.json").write_text(json.dumps(expectation))

    with pytest.raises(ParityError, match="golden test 0"):
        check(manifest, tmp_path, mapping_source)


@requires_bento
def test_a_compiler_that_emits_the_wrong_bloblang_fails(manifest, mapping_manifest,
                                                        mapping_source, monkeypatch):
    """The runner has to catch a compiler regression, not only a wrong expectation."""
    real = test_mapping_parity.compile_bloblang

    def wrong(transformation, **kwargs):
        artifact = real(transformation, **kwargs)
        artifact.mapping = artifact.mapping.replace("root.pm25 = this.pm2p5",
                                                    "root.pm25 = this.pm2p5 + 1")
        return artifact

    monkeypatch.setattr(test_mapping_parity, "compile_bloblang", wrong)
    with pytest.raises(ParityError, match="bento test failed"):
        check(manifest, mapping_manifest.parent, mapping_source)


def test_a_mapping_with_no_golden_test_is_refused(manifest, mapping_manifest, mapping_source):
    manifest["spec"]["tests"] = []
    with pytest.raises(ModelError, match="DM-39 requires at least one"):
        check(manifest, mapping_manifest.parent, mapping_source)


def test_a_mapping_with_no_transformation_is_refused(manifest, mapping_manifest,
                                                     mapping_source):
    manifest["spec"].pop("transformation")
    with pytest.raises(ModelError, match="no spec.transformation"):
        check(manifest, mapping_manifest.parent, mapping_source)


def test_a_missing_example_names_the_file(manifest, mapping_manifest, mapping_source):
    manifest["spec"]["tests"][0]["input"] = "absent.json"
    with pytest.raises(ModelError, match="absent.json does not exist"):
        check(manifest, mapping_manifest.parent, mapping_source)


# --- the command line -----------------------------------------------------------------


@requires_bento
def test_main_reports_the_fixtures_green(mapping_manifest, mapping_source, capsys):
    assert main([str(mapping_manifest), "--source-schema", mapping_source]) == 0
    assert "agree across reference, bloblang" in capsys.readouterr().out


def test_main_fails_when_bento_cannot_run(mapping_manifest, mapping_source, monkeypatch,
                                          capsys):
    """A run where the compiled mapping never executed is a failure, not a pass."""
    monkeypatch.setattr(test_mapping_parity, "bento_command", lambda: None)
    assert main([str(mapping_manifest), "--source-schema", mapping_source]) == 1
    assert "DM-39 is not satisfied" in capsys.readouterr().err


def test_main_fails_on_a_broken_mapping(tmp_path, mapping_source, capsys):
    broken = tmp_path / "broken.yaml"
    broken.write_text(yaml.safe_dump({"spec": {"transformation": {"class_derivations": {}},
                                               "tests": [{"input": "a", "expect": "b"}]}}))
    assert main([str(broken), "--source-schema", mapping_source]) == 1
    assert "broken.yaml: FAILED" in capsys.readouterr().err
