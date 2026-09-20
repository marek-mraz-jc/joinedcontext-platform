"""A draft LinkML model inferred from a sample a person dropped (T-0598, DM-54, DM-55).

The sample is a CSV, an Excel workbook, a JSON document or a PDF with tables. The answer is one
model in two forms that say the same thing: the LinkML source, and the ordered editor operations
(DM-13) that build it, so the visual editor applies them as if a person had clicked. Everything
is a guess a person corrects in the editor; nothing here publishes (CC-71).

The parse is in memory and alone (DM-55): at most `MAX_SAMPLE_BYTES`, no file written, no
network. The Excel reader is openpyxl on top of defusedxml, which refuses external entities and
entity expansion; `test_infer_schema` asserts the guard is in place rather than trusting the
install. The PDF reader resolves nothing outside the file. The Smart Data Models alignment reads
the attribute index the service already holds and fetches nothing.

A header is text from the file. It becomes a LinkML name (`NAME`) and the original travels as the
slot's `title`, a string the editor renders as text (DM-55): nothing here writes markup.
"""

from __future__ import annotations

import csv
import io
import json
import re
from collections import Counter
from datetime import datetime
from pathlib import PurePosixPath
from typing import Any

import yaml

from common import UPSTREAM_ANNOTATION

#: The cap of DM-55; the service refuses a larger body before it is read.
MAX_SAMPLE_BYTES = 10 * 1024 * 1024
#: Values read per column. A million-row export says nothing a thousand rows do not.
MAX_ROWS = 1000
#: The formats a sample may have, by extension.
FORMATS = {"csv": "csv", "tsv": "csv", "txt": "csv", "xlsx": "xlsx", "xlsm": "xlsx", "json": "json", "pdf": "pdf"}
NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
#: Slots every entity inherits from `Entity` (ngsi-ld-core); a column of that name is not declared.
CORE_SLOTS = ("id", "type", "location", "observedAt")
SDM_NAMESPACE = "https://smartdatamodels.org/"

#: UN/CEFACT common codes by the token a header carries (DM-06): `pm10_ugm3`, `temperature (°C)`,
#: `speed [km/h]`. The codes are the ones the editor offers, so an inferred unit is one it accepts.
UNITS = {
    "ug/m3": "GQ", "ugm3": "GQ",
    "mg/l": "M1", "mgl": "M1",
    "°c": "CEL", "degc": "CEL", "celsius": "CEL",
    "%": "P1", "pct": "P1", "percent": "P1",
    "m": "MTR", "metre": "MTR", "meter": "MTR",
    "km": "KMT",
    "m/s": "MTS", "mps": "MTS",
    "km/h": "KMH", "kmh": "KMH", "kph": "KMH",
    "s": "SEC", "sec": "SEC",
    "h": "HUR", "hr": "HUR",
    "kg": "KGM",
    "t": "TNE",
    "l": "LTR",
    "m3": "MTQ",
    "kwh": "KWH",
    "w": "WTT",
    "cd/m2": "A24",
    "db": "2N",
    "hpa": "HPA",
}
UNIT_UCUM = {
    "GQ": "ug/m3", "M1": "mg/L", "CEL": "Cel", "P1": "%", "MTR": "m", "KMT": "km", "MTS": "m/s",
    "KMH": "km/h", "SEC": "s", "HUR": "h", "KGM": "kg", "TNE": "t", "LTR": "L", "MTQ": "m3",
    "KWH": "kW.h", "WTT": "W", "A24": "cd/m2", "2N": "dB", "HPA": "hPa", "C62": "1",
}

#: The QUDT anchor of each unit (DM-59): the unit IRI a federated reader dereferences, and the
#: quantity kind that says what dimension is being measured, so two organisations' measurements
#: can be aligned instead of two opaque codes compared. Read out of QUDT's own vocabulary by
#: `qudt:ucumCode`, never written from memory — `ug/m3` is `MassDensity` to QUDT and not the
#: `MassConcentration` a person would guess. The editor's `UNIT_CODES` carries the same table for
#: the picker (`ui/src/pages/models/linkml.ts`, in the other repository); nothing can compare the
#: two from inside one checkout, so a code added here belongs there in the same change.
UNIT_QUDT = {
    "GQ": ("MicroGM-PER-M3", "MassDensity"),
    "M1": ("MilliGM-PER-L", "MassConcentration"),
    "CEL": ("DEG_C", "Temperature"),
    "P1": ("PERCENT", "DimensionlessRatio"),
    "MTR": ("M", "Length"),
    "KMT": ("KiloM", "Length"),
    "MTS": ("M-PER-SEC", "Speed"),
    "KMH": ("KiloM-PER-HR", "LinearVelocity"),
    "SEC": ("SEC", "Time"),
    "HUR": ("HR", "Time"),
    "KGM": ("KiloGM", "Mass"),
    "TNE": ("TONNE", "Mass"),
    "LTR": ("L", "Volume"),
    "MTQ": ("M3", "Volume"),
    "KWH": ("KiloW-HR", "Energy"),
    "WTT": ("W", "Power"),
    "A24": ("CD-PER-M2", "Luminance"),
    "2N": ("DeciB", "SoundPressureLevel"),
    "HPA": ("HectoPA", "ForcePerArea"),
    "C62": ("NUM", "Dimensionless"),
}

#: The namespaces the unit mappings cite. Declared on every inferred model that carries a unit,
#: because a CURIE under an undeclared prefix dangles in every artifact (DM-59).
UNIT_PREFIXES = {
    "ucefact": "https://vocabulary.uncefact.org/UnitMeasureCode#",
    "qudt-unit": "http://qudt.org/vocab/unit/",
    "qudt-quantkind": "http://qudt.org/vocab/quantitykind/",
}
LATITUDES = {"lat", "latitude", "y"}
LONGITUDES = {"lon", "lng", "long", "longitude", "x"}
DATE_FORMS = ("%d.%m.%Y", "%d/%m/%Y", "%m/%d/%Y", "%Y/%m/%d", "%d.%m.%Y %H:%M", "%d.%m.%Y %H:%M:%S")
TRUE_FALSE = {"true", "false", "yes", "no"}


class SampleError(ValueError):
    """The sample cannot be read as the format it claims. The message names the format, never
    a value from the file: the file may be someone's unpublished data."""


# ---------------------------------------------------------------------------------------------
# Parsing: every reader answers tables of (class name, headers, rows of cell values).


def parse(name: str, content: bytes, fmt: str | None = None) -> list[tuple[str, list[str], list[list[Any]]]]:
    """The tables of one sample, by its declared or guessed format."""
    if len(content) > MAX_SAMPLE_BYTES:
        raise SampleError(f"the sample is larger than the {MAX_SAMPLE_BYTES // (1024 * 1024)} MiB limit")
    stem = PurePosixPath(name).stem or "Sample"
    suffix = PurePosixPath(name).suffix.lstrip(".").lower()
    kind = fmt or FORMATS.get(suffix)
    if kind is None:
        # Sniff, cheaply: a workbook is a zip, a PDF says so, JSON starts with a bracket.
        head = content[:4]
        kind = "xlsx" if head.startswith(b"PK") else "pdf" if head.startswith(b"%PDF") else "json" if head.lstrip()[:1] in (b"{", b"[") else "csv"
    readers = {"csv": _csv, "xlsx": _xlsx, "json": _json, "pdf": _pdf}
    if kind not in readers:
        raise SampleError(f"'{kind}' is not a sample format: one of {', '.join(sorted(readers))}")
    tables = readers[kind](stem, content)
    return [(cls, headers, rows[:MAX_ROWS]) for cls, headers, rows in tables if headers]


def _text(content: bytes) -> str:
    try:
        return content.decode("utf-8-sig")
    except UnicodeDecodeError:
        return content.decode("latin-1")


def _csv(stem: str, content: bytes) -> list[tuple[str, list[str], list[list[Any]]]]:
    text = _text(content)
    try:
        dialect: type[csv.Dialect] | csv.Dialect = csv.Sniffer().sniff(text[:4096], delimiters=",;\t|")
    except csv.Error:
        dialect = csv.excel
    rows = [row for row in csv.reader(io.StringIO(text), dialect) if any(cell.strip() for cell in row)]
    if not rows:
        raise SampleError("the CSV has no header row")
    return [(stem, [cell.strip() for cell in rows[0]], rows[1:])]


def _xlsx(stem: str, content: bytes) -> list[tuple[str, list[str], list[list[Any]]]]:
    import openpyxl
    import openpyxl.xml

    if not openpyxl.xml.DEFUSEDXML:
        # DM-55: never parse a workbook with a reader that follows external entities.
        raise SampleError("the Excel reader has no XML entity guard; defusedxml is not installed")
    try:
        book = openpyxl.load_workbook(io.BytesIO(content), read_only=True, data_only=True)
    except Exception as err:  # noqa: BLE001 - openpyxl raises a dozen types for one meaning
        raise SampleError("the file is not an Excel workbook") from err
    tables = []
    for sheet in book.worksheets:
        rows = [list(row) for row in sheet.iter_rows(values_only=True) if any(c not in (None, "") for c in row)]
        if not rows:
            continue
        headers = ["" if cell is None else str(cell).strip() for cell in rows[0]]
        cls = sheet.title if len(book.worksheets) > 1 else stem
        tables.append((cls, headers, rows[1:]))
    book.close()
    return tables


def _json(stem: str, content: bytes) -> list[tuple[str, list[str], list[list[Any]]]]:
    try:
        document = json.loads(_text(content))
    except json.JSONDecodeError as err:
        raise SampleError("the file is not a JSON document") from err
    tables = []

    def table(cls: str, objects: list[Any]) -> None:
        records = [o for o in objects if isinstance(o, dict)]
        if not records:
            return
        headers: list[str] = []
        for record in records:
            headers.extend(k for k in record if k not in headers)
        typed = next((r["type"] for r in records if isinstance(r.get("type"), str) and NAME.match(r["type"])), None)
        tables.append((typed or cls, headers, [[r.get(h) for h in headers] for r in records]))

    if isinstance(document, list):
        table(stem, document)
    elif isinstance(document, dict):
        nested = {k: v for k, v in document.items() if isinstance(v, list) and any(isinstance(o, dict) for o in v)}
        if nested and "type" not in document:
            for key, value in nested.items():
                table(_class_name(key), value)
        else:
            table(stem, [document])
    if not tables:
        raise SampleError("the JSON holds no object to infer a class from")
    return tables


def _pdf(stem: str, content: bytes) -> list[tuple[str, list[str], list[list[Any]]]]:
    from pypdf import PdfReader

    try:
        reader = PdfReader(io.BytesIO(content))
        text = "\n".join(page.extract_text() or "" for page in reader.pages)
    except Exception as err:  # noqa: BLE001
        raise SampleError("the file is not a readable PDF") from err
    # ponytail: a text table, cells split on tabs or two spaces; the first line with two or more
    # cells is the header, every following line with the same width a row. A layout-aware table
    # extractor (pdfplumber) is the upgrade when scanned or multi-column reports show up.
    lines = [re.split(r"\t|\s{2,}", line.strip()) for line in text.splitlines() if line.strip()]
    tables = []
    headers: list[str] | None = None
    rows: list[list[Any]] = []
    for cells in lines:
        if headers is None:
            if len(cells) >= 2:
                headers = cells
            continue
        if len(cells) == len(headers):
            rows.append(cells)
        elif rows:
            tables.append((stem, headers, rows))
            headers, rows = (cells if len(cells) >= 2 else None), []
    if headers and rows:
        tables.append((stem, headers, rows))
    if not tables:
        raise SampleError("the PDF has no table this reader can see")
    return tables


# ---------------------------------------------------------------------------------------------
# Naming.


def _slot_name(header: str, index: int, taken: set[str]) -> str:
    name = re.sub(r"[^A-Za-z0-9_]+", "_", header.split("(")[0].split("[")[0].strip()).strip("_")
    if not name:
        name = f"column_{index + 1}"
    if name[0].isdigit():
        name = f"_{name}"
    base, n = name, 2
    while name in taken:
        name, n = f"{base}_{n}", n + 1
    taken.add(name)
    return name


def _class_name(text: str) -> str:
    words = re.findall(r"[A-Za-z0-9]+", text)
    name = "".join(w[:1].upper() + w[1:] for w in words) or "Sample"
    return f"_{name}" if name[0].isdigit() else name


def _unit(header: str) -> tuple[str | None, str]:
    """The UN/CEFACT code a header carries and the header without it: `pm10_ugm3` is `pm10` in
    GQ, `temperature (°C)` is `temperature` in CEL. A bare one-letter token (`m`, `s`, `t`) is a
    unit only in brackets, because `speed m` is a guess and `speed (m)` a statement."""
    text = header.strip()
    bracketed = re.search(r"\s*[(\[]([^)\]]+)[)\]]\s*$", text)
    token = bracketed.group(1) if bracketed else re.split(r"[\s_]+", text)[-1] if re.search(r"[\s_]", text) else ""
    key = token.strip().lower().replace("\u00b5", "u").replace("\u03bc", "u").replace("³", "3").replace("²", "2")
    code = UNITS.get(key)
    if code is None or (len(key) == 1 and key != "%" and not bracketed):
        return None, text
    return code, text[: bracketed.start()] if bracketed else re.split(r"[\s_]+" + re.escape(token) + r"$", text)[0]


# ---------------------------------------------------------------------------------------------
# Typing.


def _as_datetime(value: str) -> str | None:
    """`date` or `datetime` when the text is one of the forms people export, else None."""
    text = value.strip()
    if not text:
        return None
    try:
        datetime.fromisoformat(text.replace("Z", "+00:00"))
        return "date" if len(text) == 10 else "datetime"
    except ValueError:
        pass
    for form in DATE_FORMS:
        try:
            datetime.strptime(text, form)
            return "datetime" if "%H" in form else "date"
        except ValueError:
            continue
    return None


def _shape(value: str) -> str:
    return re.sub(r"[a-z]", "a", re.sub(r"[A-Z]", "A", re.sub(r"[0-9]", "9", value)))


def _pattern(shape: str) -> str:
    out, i = "", 0
    while i < len(shape):
        run = 1
        while i + run < len(shape) and shape[i + run] == shape[i]:
            run += 1
        atom = {"9": "[0-9]", "A": "[A-Z]", "a": "[a-z]"}.get(shape[i], re.escape(shape[i]))
        out += atom + (f"{{{run}}}" if run > 1 else "")
        i += run
    return f"^{out}$"


def _classify(value: Any) -> tuple[str, Any]:
    """One cell as (kind, normalised value): integer, float, boolean, datetime, date, urn, uri,
    geo, object, list, text; or empty."""
    if value is None or (isinstance(value, str) and not value.strip()):
        return "empty", None
    if isinstance(value, bool):
        return "boolean", value
    if isinstance(value, int):
        return "integer", value
    if isinstance(value, float):
        return "float", value
    if isinstance(value, datetime):
        return "datetime", value
    if isinstance(value, dict):
        if isinstance(value.get("type"), str) and "coordinates" in value:
            return "geo", value
        return "object", value
    if isinstance(value, list):
        return "list", value
    text = str(value).strip()
    lowered = text.lower()
    if lowered in TRUE_FALSE:
        return "boolean", lowered in ("true", "yes")
    if re.fullmatch(r"[+-]?\d+", text):
        return "integer", int(text)
    if re.fullmatch(r"[+-]?(\d+[.,]\d*|[.,]\d+)([eE][+-]?\d+)?", text):
        return "float", float(text.replace(",", "."))
    if text.startswith("urn:ngsi-ld:"):
        return "urn", text
    if re.match(r"https?://\S+$", text):
        return "uri", text
    if re.match(r"^(POINT|LINESTRING|POLYGON|MULTI\w+)\s*\(", text, re.IGNORECASE):
        return "geo", text
    when = _as_datetime(text)
    if when:
        return when, text
    return "text", text


def _column(values: list[Any]) -> dict[str, Any]:
    """What one column's values say: the range, the kind, bounds, a pattern, or why not."""
    kinds = Counter()
    numbers: list[float] = []
    shapes: set[str] = set()
    for raw in values:
        kind, value = _classify(raw)
        if kind == "empty":
            continue
        kinds[kind] += 1
        if kind in ("integer", "float"):
            numbers.append(float(value))
        elif kind == "text":
            shapes.add(_shape(value))
    seen = sum(kinds.values())
    if seen == 0:
        return {"range": "string", "untyped": "every value is empty"}
    # A column of integers with the odd decimal is a float column; a date column with the odd
    # timestamp is a datetime column. Anything else mixed stays text and is named as such.
    if set(kinds) <= {"integer", "float"}:
        if set(kinds) == {"integer"}:
            return {"range": "integer", "minimum_value": int(min(numbers)), "maximum_value": int(max(numbers))}
        return {"range": "float", "minimum_value": min(numbers), "maximum_value": max(numbers)}
    if set(kinds) <= {"date", "datetime"}:
        return {"range": "datetime" if "datetime" in kinds else "date"}
    if len(kinds) > 1:
        detail = ", ".join(f"{n} {k}" for k, n in kinds.most_common())
        return {"range": "string", "untyped": f"mixed values: {detail}"}
    (kind,) = kinds
    if kind == "boolean":
        return {"range": "boolean"}
    if kind == "urn":
        return {"range": "uriorcurie", "kind": "Relationship"}
    if kind == "uri":
        return {"range": "uri"}
    if kind == "geo":
        return {"range": "string", "kind": "GeoProperty"}
    if kind == "object":
        return {"range": "string", "kind": "JsonProperty"}
    if kind == "list":
        return {"range": "string", "kind": "ListProperty"}
    column: dict[str, Any] = {"range": "string"}
    # One obvious form: every value has the same shape, it carries a digit, and it is short.
    if len(shapes) == 1 and seen >= 2:
        shape = next(iter(shapes))
        if "9" in shape and len(shape) <= 12:
            column["pattern"] = _pattern(shape)
    return column


# ---------------------------------------------------------------------------------------------
# The model.


def infer(
    name: str,
    content: bytes,
    fmt: str | None = None,
    index: dict[str, list[str]] | None = None,
) -> dict[str, Any]:
    """The answer of `POST /infer-schema` (API/01 §11): `linkml`, `operations`, `detectedTypes`,
    `matches`, `untyped`, `rows`, `errors`.

    `index` maps a Smart Data Models attribute name to the catalogue models declaring it
    (`dataModel.Environment/AirQualityObserved`), read from the service's cache: a slot of that
    name binds the catalogue's IRI as `slot_uri` with the upstream citation the squatting guard
    asks for (DM-04, DM-16). The operations leave the IRI out, because the editor refuses a
    reserved IRI through an operation; the source carries it and `matches` names it.
    """
    tables = parse(name, content, fmt)
    index = index or {}
    classes: dict[str, Any] = {}
    slots: dict[str, Any] = {}
    operations: list[dict[str, Any]] = []
    detected: dict[str, str] = {}
    matches: dict[str, dict[str, str]] = {}
    untyped: list[dict[str, str]] = []
    rows = 0
    taken_classes: set[str] = set()

    for cls, headers, records in tables:
        cls = _class_name(cls)
        while cls in taken_classes:
            cls += "_"
        taken_classes.add(cls)
        rows += len(records)
        columns = {i: [r[i] if i < len(r) else None for r in records] for i in range(len(headers))}
        lowered = [h.strip().lower() for h in headers]
        # A latitude/longitude pair is the entity's `location`, one GeoProperty (DM-54).
        lat = next((i for i, h in enumerate(lowered) if h in LATITUDES), None)
        lon = next((i for i, h in enumerate(lowered) if h in LONGITUDES), None)
        skip: set[int] = set()
        if lat is not None and lon is not None:
            skip |= {lat, lon}
            detected["location"] = "GeoProperty"
        class_slots: list[str] = []
        taken = set(slots)
        operations.append({"op": "addClass", "name": cls, "is_a": "Entity"})
        for i, header in enumerate(headers):
            if i in skip or not header or header == "@context":
                continue
            column = _column(columns[i])
            if header in CORE_SLOTS:
                # Inherited from Entity: recorded as seen, never redeclared.
                detected[header] = "GeoProperty" if header == "location" else column["range"]
                continue
            unit, bare = _unit(header)
            slot = _slot_name(bare, i, taken)
            definition: dict[str, Any] = {"range": column["range"]}
            if header != slot:
                definition["title"] = {"en": header}
            for key in ("pattern", "minimum_value", "maximum_value"):
                if key in column:
                    definition[key] = column[key]
            if column.get("kind"):
                definition["annotations"] = {"ngsi_ld_kind": column["kind"]}
            if unit:
                qudt_unit, quantity_kind = UNIT_QUDT[unit]
                definition["unit"] = {
                    "ucum_code": UNIT_UCUM[unit],
                    "exact_mappings": [f"ucefact:{unit}", f"qudt-unit:{qudt_unit}"],
                    "has_quantity_kind": f"qudt-quantkind:{quantity_kind}",
                }
            models = index.get(slot) or []
            if models:
                subject = models[0].split("/")[0]
                definition["slot_uri"] = f"{SDM_NAMESPACE}{subject}/{slot}"
                definition.setdefault("annotations", {})[UPSTREAM_ANNOTATION] = models[0]
                matches[slot] = {"model": models[0], "slotUri": definition["slot_uri"]}
            slots[slot] = definition
            class_slots.append(slot)
            detected[slot] = column.get("kind") or column["range"]
            if "untyped" in column:
                untyped.append({"slot": slot, "reason": column["untyped"]})

            add: dict[str, Any] = {"op": "addSlot", "name": slot, "class": cls, "range": column["range"]}
            if column.get("kind"):
                add["kind"] = column["kind"]
            operations.append(add)
            if header != slot:
                operations.append({"op": "setTitle", "target": "slot", "name": slot, "locale": "en", "value": header})
            for key in ("pattern", "minimum_value", "maximum_value"):
                if key in column:
                    operations.append({"op": "setSlot", "name": slot, "field": key, "value": column[key]})
            if unit:
                operations.append({"op": "setSlot", "name": slot, "field": "unit", "value": unit})
        classes[cls] = {"is_a": "Entity", "slots": class_slots}
        suggested = Counter(m["model"] for s, m in matches.items() if s in class_slots).most_common(1)
        if suggested and suggested[0][1] * 2 >= max(len(class_slots), 1):
            classes[cls]["description"] = f"Adapted from {suggested[0][0]}: {suggested[0][1]} of {len(class_slots)} attributes match."
            operations.append({"op": "setClass", "name": cls, "field": "description", "value": classes[cls]["description"]})

    prefixes = {"linkml": "https://w3id.org/linkml/"}
    if matches:
        prefixes["sdm"] = SDM_NAMESPACE
    if any("unit" in definition for definition in slots.values()):
        prefixes.update(UNIT_PREFIXES)
    stem = _class_name(PurePosixPath(name).stem)
    document = {
        "id": f"https://example.org/models/{stem}",
        "name": stem,
        "prefixes": prefixes,
        "default_range": "string",
        "imports": ["linkml:types", "ngsi-ld-core"],
        "classes": classes,
        "slots": slots,
    }
    return {
        "linkml": yaml.safe_dump(document, sort_keys=False, allow_unicode=True),
        "operations": operations,
        "detectedTypes": detected,
        "matches": matches,
        "untyped": untyped,
        "rows": rows,
        "errors": [],
    }
