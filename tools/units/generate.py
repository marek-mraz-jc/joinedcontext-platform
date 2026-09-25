#!/usr/bin/env python3
"""Generate the one UN/CEFACT unit code list, `crates/jc-core/data/unece-rec20.json` (DM-06, DM-59).

The list is UNECE Recommendation 20 as UN/CEFACT publishes it, joined with QUDT's own
`qudt:uneceCommonCode`, `qudt:ucumCode`, `qudt:hasQuantityKind`, `qudt:hasDimensionVector`,
`qudt:conversionMultiplier` and `qudt:conversionOffset`. Nobody maps a code by hand: a join
that QUDT does not make stays empty, and a code QUDT gives two units that convert differently
keeps the one whose symbol is Rec 20's, or no QUDT fields at all rather than a guessed one,
because a wrong factor is a wrong number in every conversion.

Both sources are pinned by URL and SHA-256 below and fetched once, here; nothing reads them at
runtime. The output is deterministic, so a copy is checked by comparing bytes: the source is
written first, then every copy named on the command line (Model Tools' is always written).

    pip install ./tools/model-tools          # rdflib, pinned there
    python tools/units/generate.py [more copy paths...]
"""

from __future__ import annotations

import csv
import hashlib
import io
import json
import sys
import urllib.request
import zipfile
from pathlib import Path

from rdflib import Graph, Namespace
from rdflib.namespace import SKOS

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "crates/jc-core/data/unece-rec20.json"
COPIES = [ROOT / "tools/model-tools/models/unece-rec20.json"]

REC20 = {
    "revision": "Recommendation 20, Revision 17 (2021), Annexes II and III",
    "url": "https://raw.githubusercontent.com/uncefact/vocab-codes/"
    "9db5053a041ebb5f53de471a5975528e8aa51541/CLR/Rec20/current/"
    "Annex%20%7C%7C%20%26%20Annex%20%7C%7C%7C/code-list.csv",
    "sha256": "74f4953ad032e88b2370820682021d2ebd3840f9ff79a82c1cc0301e87bdb6f5",
}
QUDT = {
    "revision": "QUDT v3.5.2 (2026-09-22)",
    "url": "https://github.com/qudt/qudt-public-repo/releases/download/v3.5.2/qudt-public-repo-3.5.2.zip",
    "sha256": "1a42d6409f57e2d8fc4f55aac5e4378926c28a655a045309714f0014fcea3e02",
}

#: The municipal set a picker shows first (DM-06): air quality, weather, traffic, energy,
#: water and counts. A choice of which codes to show first, not a mapping: every field of these
#: entries still comes from the two sources.
FREQUENT = {
    "GQ", "GP", "M1", "CEL", "KEL", "P1", "MTR", "KMT", "MMT", "MTK", "MTS", "KMH", "SEC",
    "MIN", "HUR", "KGM", "TNE", "LTR", "MTQ", "MQH", "KWH", "KWT", "WTT", "A24", "2N", "A97",
    "C62", "H87",
}

Q = Namespace("http://qudt.org/schema/qudt/")
UNITS_TTL = "vocab/unit/VOCAB_QUDT-UNITS-ALL.ttl"
KINDS_TTL = "vocab/quantitykinds/VOCAB_QUDT-QUANTITY-KINDS-ALL.ttl"


def fetch(source: dict[str, str]) -> bytes:
    with urllib.request.urlopen(source["url"], timeout=120) as response:
        body = response.read()
    digest = hashlib.sha256(body).hexdigest()
    if digest != source["sha256"]:
        raise SystemExit(f"{source['url']}: SHA-256 {digest}, pinned {source['sha256']}")
    return body


def local(iri: object) -> str:
    return str(iri).rsplit("/", 1)[-1]


def number(value: object) -> float | None:
    return None if value is None else float(str(value))


def qudt_units(archive: bytes) -> dict[str, list[dict]]:
    """Every QUDT unit that names a Rec 20 code, by code."""
    bundle = zipfile.ZipFile(io.BytesIO(archive))
    units = Graph().parse(data=bundle.read(UNITS_TTL).decode(), format="turtle")
    kinds = Graph().parse(data=bundle.read(KINDS_TTL).decode(), format="turtle")

    candidates: dict[str, list[dict]] = {}
    for unit, code in units.subject_objects(Q.uneceCommonCode):
        if str(units.value(unit, Q.deprecated) or "").lower() == "true":
            continue
        declared = set(units.objects(unit, Q.hasQuantityKind))
        # The broadest kinds only: `Temperature`, not also `BoilingPoint` and `FlashPoint`.
        roots = sorted(
            local(k) for k in declared if not (set(kinds.objects(k, SKOS.broader)) & declared)
        )
        multiplier = number(units.value(unit, Q.conversionMultiplier))
        dimension = units.value(unit, Q.hasDimensionVector)
        candidates.setdefault(str(code).strip(), []).append({
            "qudt": local(unit),
            "ucum": sorted(str(u) for u in units.objects(unit, Q.ucumCode)),
            "quantityKinds": roots,
            "dimension": local(dimension) if dimension is not None else None,
            # QUDT writes 0 for a logarithmic unit (decibel): not a factor anything converts by.
            "factor": multiplier if multiplier else None,
            "offset": number(units.value(unit, Q.conversionOffset)) or 0.0,
            "symbol": str(units.value(unit, Q.symbol) or ""),
        })

    return candidates


def conversion(unit: dict) -> tuple:
    return unit["factor"], unit["offset"], unit["dimension"]


def join(code: str, rec20_symbol: str, found: list[dict]) -> dict:
    """The one QUDT unit of a code, or none when QUDT's candidates convert differently.

    Where they disagree, the candidates whose symbol is Rec 20's own decide (`ms` picks
    `MilliSEC` over `DeciSEC` for `C26`); when that still leaves two conversions, none is taken.
    """
    if not found:
        return {}
    if len({conversion(c) for c in found}) > 1:
        found = [c for c in found if c["symbol"] and c["symbol"] == rec20_symbol]
        if len({conversion(c) for c in found}) != 1:
            return {}
    # The same conversion under two names (`ONE` and `UNITLESS` for `C62`): take the first name,
    # so a regeneration answers the same one.
    return min(found, key=lambda c: c["qudt"])


def symbol_of(rec20: str, qudt: str) -> str:
    """Rec 20's symbol, its first spelling where it offers two (`% or pct`); QUDT's otherwise."""
    written = rec20.split(" or ")[0].strip()
    return written or qudt


def build(rec20: bytes, qudt: dict[str, list[dict]]) -> dict:
    rows = csv.DictReader(io.StringIO(rec20.decode("utf-8")))
    units = []
    for row in rows:
        code = row["CommonCode"].strip()
        status = row["Status"].strip()
        if not code or status == "X":  # X: deleted from the recommendation
            continue
        joined = join(code, row["Symbol"].strip(), qudt.get(code, []))
        units.append({
            "code": code,
            "name": " ".join(row["Name"].split()),
            "symbol": symbol_of(row["Symbol"], joined.get("symbol", "")),
            "deprecated": status == "D",
            "ucum": (joined.get("ucum") or [None])[0],
            "qudt": joined.get("qudt"),
            "quantityKinds": joined.get("quantityKinds", []),
            "dimension": joined.get("dimension"),
            "factor": joined.get("factor"),
            "offset": joined.get("offset", 0.0),
            "frequent": code in FREQUENT,
        })
    units.sort(key=lambda u: u["code"])
    codes = [u["code"] for u in units]
    if len(codes) != len(set(codes)):
        raise SystemExit("Rec 20 lists a code twice")
    missing = FREQUENT - set(codes)
    if missing:
        raise SystemExit(f"frequent codes not in Rec 20: {sorted(missing)}")
    return {"source": {"rec20": REC20, "qudt": QUDT}, "units": units}


def render(document: dict) -> str:
    """One unit per line, so a regeneration diffs by code."""
    lines = ",\n".join("    " + json.dumps(u, ensure_ascii=False) for u in document["units"])
    source = json.dumps(document["source"], ensure_ascii=False, indent=2).replace("\n", "\n  ")
    return f'{{\n  "source": {source},\n  "units": [\n{lines}\n  ]\n}}\n'


def main(extra: list[str]) -> None:
    text = render(build(fetch(REC20), qudt_units(fetch(QUDT))))
    for path in [SOURCE, *COPIES, *map(Path, extra)]:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        print(f"wrote {path}")


if __name__ == "__main__":
    main(sys.argv[1:])
