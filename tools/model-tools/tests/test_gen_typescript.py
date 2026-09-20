"""The `jc-types.ts` a generated application compiles against (T-0677, SDK-03, SDK-10)."""

from __future__ import annotations

import shutil
import subprocess

import pytest

from common import ModelError
from gen_typescript import compile_typescript, main

#: Every range and kind the row mapping treats differently, in one model beside the fixture's.
KINDS = """
id: https://example.org/ns/kinds
name: kinds
prefixes:
  linkml: https://w3id.org/linkml/
  ex: https://example.org/ns/
default_prefix: ex
default_range: string
imports:
  - linkml:types
  - ngsi-ld-core
classes:
  Counter:
    description: Counts things. A comment that closes */ early must not end the doc block.
    is_a: Entity
    slots: [count, share, open, tags, dateObserved, second]
  Base:
    abstract: true
    is_a: Entity
  Plain:
    is_a: Entity
slots:
  count: {range: integer, required: true}
  share: {range: decimal}
  open: {range: boolean}
  tags: {range: string, multivalued: true}
  dateObserved: {range: datetime}
  second: {range: string, alias: 2ndReading}
"""

#: The SDK's own row type, as `sdk/src/ngsi.ts` declares it: a generated type that is not
#: assignable to this cannot be handed to `useEntities<T>`.
USAGE = """
import type { AirQualityObserved, EntityTypeName, FreeForm, ReliabilityLevel } from "./jc-types";
import type { Counter } from "./kinds";
interface Geo { type: string; coordinates: unknown }
type Cell = string | number | boolean | Geo | null;
type Row = { id: string; type: string } & Record<string, Cell>;
declare function list<T extends Row = Row>(type: EntityTypeName | "Counter"): T[];
const air = list<AirQualityObserved>("AirQualityObserved")[0];
const celsius: number = air.temperature;
const level: ReliabilityLevel | null | undefined = air.reliability;
const free = list<FreeForm>("FreeForm")[0];
const other: Cell = free["anything"];
const counter = list<Counter>("Counter")[0];
const count: number = counter.count;
export { celsius, level, other, count };
"""


def test_every_ngsi_ld_kind_becomes_the_cell_the_sdk_returns(senzor):
    ts = compile_typescript(senzor)

    assert 'type: "AirQualityObserved";' in ts
    assert "  id: string;" in ts
    assert "  temperature: number;" in ts  # required, so never absent
    assert "  reliability?: ReliabilityLevel | null;" in ts
    assert "  refDevice?: string | null;" in ts  # Relationship: its URN
    assert "  label?: string | null;" in ts  # LanguageProperty: the text in one language
    assert "  address?: string | null;" in ts  # inline object: the text the table shows
    assert "  location?: Geometry | null;" in ts  # GeoProperty, inherited from ngsi-ld-core
    assert "  observedAt?: string | null;" in ts  # DateTime: its ISO string
    assert "Unit: °C." in ts


def test_the_file_is_types_only_so_import_type_erases_all_of_it(senzor):
    ts = compile_typescript(senzor)

    assert 'export type ReliabilityLevel = "low" | "medium" | "high";' in ts
    for runtime in ("export enum", "export function", "export const", "export class", "import "):
        assert runtime not in ts


def test_only_entity_types_are_declared_and_open_world_accepts_any_attribute(senzor):
    ts = compile_typescript(senzor)

    assert "export type Entity " not in ts
    assert "export type Address " not in ts
    assert 'export type EntityTypeName = "AirQualityObserved" | "FreeForm";' in ts
    assert "} & { [attr: string]: string | number | boolean | Geometry | null };" in ts
    # Only the open-world class gets the intersection.
    assert ts.count("[attr: string]") == 1


def test_ranges_lists_and_awkward_names():
    ts = compile_typescript(KINDS)

    assert "  count: number;" in ts
    assert "  share?: number | null;" in ts
    assert "  open?: boolean | null;" in ts
    assert "  tags?: string | null;" in ts  # the SDK joins a list into one text
    assert "  dateObserved?: string | null;" in ts
    assert '  "2ndReading"?: string | null;' in ts
    assert "*\\/ early" in ts and "*/ early" not in ts
    assert "export type Base " not in ts  # abstract: an endpoint serves no Base entities


def test_a_class_named_like_a_declared_type_is_refused():
    with pytest.raises(ModelError, match="Geometry"):
        compile_typescript(KINDS.replace("  Plain:", "  Geometry:"))


def test_the_command_line_writes_the_file(senzor, tmp_path):
    out = tmp_path / "jc-types.ts"

    assert main([senzor, "-o", str(out)]) == 0
    assert out.read_text() == compile_typescript(senzor)


@pytest.mark.skipif(shutil.which("tsc") is None, reason="no TypeScript compiler on PATH")
def test_the_output_compiles_and_satisfies_the_sdk_row_type(senzor, tmp_path):
    (tmp_path / "jc-types.ts").write_text(compile_typescript(senzor))
    (tmp_path / "kinds.ts").write_text(compile_typescript(KINDS))
    (tmp_path / "usage.ts").write_text(USAGE)

    result = subprocess.run(
        ["tsc", "--strict", "--noEmit", "--target", "ES2022", "--module", "ESNext",
         "--moduleResolution", "bundler", str(tmp_path / "usage.ts")],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stdout + result.stderr
