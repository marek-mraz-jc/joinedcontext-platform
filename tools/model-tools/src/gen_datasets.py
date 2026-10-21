"""LinkML → a valid and an invalid test dataset, and the proof the artifacts agree on both (T-5066).

`gen_example` commits one entity per model (DM-02). A model's constraints are only tested when
something breaks them, so this generator makes two datasets of every entity class:

- **valid**: the full example, then one record per edge it can find: each bound of a number at
  its minimum and its maximum, every value of every enum, each optional slot left out, a
  language map in more than one language, and each geometry type GeoJSON names.
- **invalid**: one record per constraint, each breaking exactly one: a required slot left out,
  a number past its bound, a string that misses its pattern, a value no enum permits, the wrong
  datatype, and an attribute a closed class does not declare. Each names the slot it breaks.

`check` holds both datasets to the SHACL shapes (with pySHACL, as a consumer validates, DM-46)
and to the JSON Schema of the class (what the gateway and the Portal hold a write to, DM-86).
Every valid record must pass both; every invalid one must fail both, and SHACL must name the
broken slot as the result path. A disagreement is a defect in a generator or in the model, and
the CLI exits 1 with each one listed.
"""

from __future__ import annotations

import argparse
import copy
import json
import logging
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from linkml_runtime import SchemaView
from linkml_runtime.linkml_model.meta import ClassDefinition, SlotDefinition

from common import ModelError, annotation_value, load, ngsi_ld_kind, slots_of, uri_of
from gen_context import compile_context
from gen_example import _domain, _object, _scalar, _urn
from gen_json_schema import compile_schema
from gen_rdf_artifacts import compile_shacl

# rdflib logs a traceback for each ill-typed literal it parses; the datatype records are made of
# them on purpose, and pySHACL reports each as the violation it is.
logging.getLogger("rdflib.term").setLevel(logging.CRITICAL)

#: Every geometry a GeoProperty may hold (RFC 7946 §3.1), each the smallest valid one.
GEOMETRIES: list[dict[str, Any]] = [
    {"type": "Point", "coordinates": [19.15, 48.74]},
    {"type": "MultiPoint", "coordinates": [[19.15, 48.74], [19.16, 48.75]]},
    {"type": "LineString", "coordinates": [[19.15, 48.74], [19.16, 48.75]]},
    {"type": "MultiLineString", "coordinates": [[[19.15, 48.74], [19.16, 48.75]]]},
    {"type": "Polygon", "coordinates": [[[19.15, 48.74], [19.16, 48.74], [19.16, 48.75], [19.15, 48.74]]]},
    {
        "type": "MultiPolygon",
        "coordinates": [[[[19.15, 48.74], [19.16, 48.74], [19.16, 48.75], [19.15, 48.74]]]],
    },
]

#: A language map in two of the languages the platform offers, one with diacritics.
LANGUAGES = {"en": "Air quality", "sk": "Kvalita ovzdušia"}

#: A value no enum of any model permits.
NOT_PERMITTED = "not-a-permitted-value"

#: Base types whose values are numbers, for bounds and the wrong-datatype record.
NUMERIC = {"integer", "float", "double", "decimal"}

#: Base types SHACL and the JSON Schema both type strictly (`sh:datatype`, `type`), so a string
#: where one is expected is refused by both.
TYPED = NUMERIC | {"boolean"}


@dataclass
class Invalid:
    """One record breaking exactly one constraint."""

    record: dict[str, Any]
    #: The slot (its JSON name) the record breaks.
    path: str
    #: What it breaks, in words a report reader knows: `required`, `minimum`, `pattern` …
    constraint: str


@dataclass
class Datasets:
    valid: list[dict[str, Any]] = field(default_factory=list)
    invalid: list[Invalid] = field(default_factory=list)


def _base(view: SchemaView, name: str) -> str:
    """The LinkML base type a range resolves to (`Celsius` typeof float → float)."""
    seen: set[str] = set()
    while name in view.all_types() and name not in seen:
        seen.add(name)
        declared = view.all_types()[name]
        name = declared.typeof or declared.base or name
        if declared.typeof is None:
            break
    return {
        "str": "string",
        "int": "integer",
        "Decimal": "decimal",
        "bool": "boolean",
        "XSDDateTime": "datetime",
        "URI": "uri",
        "URIorCURIE": "uriorcurie",
    }.get(name, name)


def entity_classes(view: SchemaView) -> list[ClassDefinition]:
    """Every class the model declares an NGSI-LD entity type of."""
    return [
        cls
        for cls in view.all_classes().values()
        if cls.name != "Entity" and "Entity" in view.class_ancestors(cls.name) and not cls.abstract
    ]


def _number(base: str, value: float) -> int | float:
    return int(value) if base == "integer" else float(value)


def _plain(slot: SlotDefinition, kind: str) -> bool:
    """A slot whose value is one scalar, so a bound or a pattern applies to it directly."""
    return kind in ("Property", "VocabProperty") and not slot.multivalued


def sample(pattern: str) -> str | None:
    """The shortest string `re.search(pattern)` finds whole, read off the parsed pattern.

    Each repeat at its least count (at least once), each class and each branch by its first
    member; None for a construct it cannot spell (a lookaround, a backreference).
    """
    from re import _constants as c  # the standard library's own regex parser
    from re import _parser as parser

    def spell(items) -> str | None:
        out = ""
        for op, arg in items:
            if op is c.LITERAL:
                out += chr(arg)
            elif op is c.ANY:
                out += "a"
            elif op is c.IN:
                members = [m for m in arg if m[0] is not c.NEGATE]
                if len(members) != len(arg):
                    out += next(ch for ch in "a0A_ -" if not re.fullmatch(f"[{''.join(_class_text(members))}]", ch))
                    continue
                kind, value = members[0]
                out += chr(value) if kind is c.LITERAL else chr(value[0]) if kind is c.RANGE else _category(value)
            elif op is c.CATEGORY:
                out += _category(arg)
            elif op in (c.MAX_REPEAT, c.MIN_REPEAT, c.POSSESSIVE_REPEAT):
                low, high, body = arg
                part = spell(body)
                if part is None:
                    return None
                out += part * max(low, min(1, high))
            elif op is c.SUBPATTERN:
                part = spell(arg[-1])
                if part is None:
                    return None
                out += part
            elif op is c.BRANCH:
                part = spell(arg[1][0])
                if part is None:
                    return None
                out += part
            elif op is c.AT:
                continue
            else:
                return None
        return out

    try:
        found = spell(parser.parse(pattern))
    except (re.error, StopIteration):
        return None
    return found if found is not None and re.search(pattern, found) else None


def _category(name) -> str:
    from re import _constants as c

    return {c.CATEGORY_DIGIT: "0", c.CATEGORY_SPACE: " ", c.CATEGORY_WORD: "a"}.get(name, "a")


def _class_text(members) -> list[str]:
    from re import _constants as c

    text = []
    for kind, value in members:
        if kind is c.LITERAL:
            text.append(re.escape(chr(value)))
        elif kind is c.RANGE:
            text.append(f"{re.escape(chr(value[0]))}-{re.escape(chr(value[1]))}")
        elif kind is c.CATEGORY:
            text.append({c.CATEGORY_DIGIT: r"\d", c.CATEGORY_SPACE: r"\s", c.CATEGORY_WORD: r"\w"}.get(value, ""))
    return text


#: Strings one of which most patterns refuse: empty, markup, punctuation, a long run.
BREAKERS = ["", "<>", "!", " ", "~" * 1001, "0", "a"]


def breaker(pattern: str) -> str | None:
    """A string the pattern does not find (SHACL and JSON Schema both search, neither anchors)."""
    return next((one for one in BREAKERS if not re.search(pattern, one)), None)


def _repair(view: SchemaView, cls: ClassDefinition, record: dict[str, Any]) -> None:
    """Give a slot the example could not fill well a value its constraints accept.

    `gen_example` spells a pattern it cannot read and a `uri` as the slot name, and a union range
    as an empty object; a valid dataset needs the value the model actually allows.
    """
    for slot in slots_of(view, cls):
        name = slot.alias or slot.name
        if name not in record or not _plain(slot, ngsi_ld_kind(slot)):
            continue
        if slot.any_of and isinstance(record[name], dict):
            first = slot.any_of[0].range
            if first:
                record[name] = _scalar(view, slot, first)
        base = _base(view, slot.range or view.schema.default_range or "string")
        if base in ("uri", "uriorcurie") and isinstance(record[name], str) and "://" not in record[name]:
            # The write path checks `format: uri`; a slot name is no URI.
            record[name] = f"https://example.org/{name}"
        if slot.pattern and isinstance(record[name], str) and not re.search(slot.pattern, record[name]):
            found = sample(slot.pattern)
            if found is not None:
                record[name] = found


def _datasets_of(view: SchemaView, cls: ClassDefinition, domain: str, out: Datasets) -> None:
    full = {"id": _urn(cls.name, domain), "type": cls.name, **_object(view, cls, domain, 0)}
    _repair(view, cls, full)
    out.valid.append(full)
    enums = view.all_enums()
    closed = annotation_value(cls, "open_world") != "true"

    def variant(name: str, value: Any) -> dict[str, Any]:
        record = copy.deepcopy(full)
        record[name] = value
        return record

    for slot in slots_of(view, cls):
        name = slot.alias or slot.name
        if name in ("id", "type") or name not in full:
            continue
        kind = ngsi_ld_kind(slot)
        range_name = slot.range or view.schema.default_range or "string"
        base = _base(view, range_name)
        plain = _plain(slot, kind)

        if slot.required:
            record = copy.deepcopy(full)
            del record[name]
            out.invalid.append(Invalid(record, name, "required"))
        else:
            record = copy.deepcopy(full)
            del record[name]
            out.valid.append(record)

        if plain and base in NUMERIC:
            if slot.minimum_value is not None:
                out.valid.append(variant(name, _number(base, slot.minimum_value)))
                out.invalid.append(Invalid(variant(name, _number(base, slot.minimum_value - 1)), name, "minimum"))
            if slot.maximum_value is not None:
                out.valid.append(variant(name, _number(base, slot.maximum_value)))
                out.invalid.append(Invalid(variant(name, _number(base, slot.maximum_value + 1)), name, "maximum"))
        if plain and base in TYPED and range_name not in enums:
            out.invalid.append(Invalid(variant(name, "not a " + base), name, "datatype"))
        broken = breaker(slot.pattern) if plain and slot.pattern and range_name not in enums else None
        if broken is not None:
            out.invalid.append(Invalid(variant(name, broken), name, "pattern"))
        if range_name in enums and (plain or kind == "Property"):
            values = list(enums[range_name].permissible_values)
            wrap = (lambda v: [v]) if slot.multivalued else (lambda v: v)
            out.valid.extend(variant(name, wrap(value)) for value in values[1:])
            out.invalid.append(Invalid(variant(name, wrap(NOT_PERMITTED)), name, "enum"))
        if kind == "LanguageProperty" and not slot.multivalued:
            out.valid.append(variant(name, dict(LANGUAGES)))
        if kind == "GeoProperty" and not slot.multivalued:
            out.valid.extend(variant(name, dict(geometry)) for geometry in GEOMETRIES[1:])

    if closed:
        out.invalid.append(Invalid(variant("undeclaredAttribute", "x"), "undeclaredAttribute", "closed"))


def compile_datasets(source: str | Path) -> Datasets:
    """Both datasets of every entity class of one LinkML document."""
    view = load(source)
    domain = _domain(view.schema.id or "")
    out = Datasets()
    classes = entity_classes(view)
    if not classes:
        raise ModelError("no class of this model specialises `Entity`, so there is nothing to test (DM-09)")
    for cls in classes:
        _datasets_of(view, cls, domain, out)
    return out


@dataclass
class Verdict:
    shacl_ok: bool
    shacl_paths: list[str]
    schema_ok: bool


class Checker:
    """The SHACL shapes, the context and the JSON Schema of one model, compiled once."""

    def __init__(self, source: str | Path):
        from rdflib import Graph

        view = load(source)
        domain = _domain(view.schema.id or "")
        # Every entity a record refers to is `_urn(class)`: asserting each one's type stands in
        # for the referenced entities the space holds, which `sh:class` on a Relationship reads.
        self.referenced = [
            (_urn(cls.name, domain), uri_of(view, cls)) for cls in view.all_classes().values()
        ]
        self.context = compile_context(source)["@context"]
        self.shapes = Graph().parse(data=compile_shacl(source), format="turtle")
        self.schema = compile_schema(source)

    def _graph(self, record: dict[str, Any]):
        # `id` and `type` are JSON-LD keywords in the NGSI-LD core context, which every response
        # carries beside the model context (as tests/test_gen_shacl.py expands them).
        from rdflib import RDF, Graph, URIRef

        payload = {"@context": [{"id": "@id", "type": "@type"}, self.context], **record}
        graph = Graph().parse(data=json.dumps(payload), format="json-ld")
        for iri, class_uri in self.referenced:
            if iri != record["id"]:
                graph.add((URIRef(iri), RDF.type, URIRef(class_uri)))
        return graph

    def _term(self, iri: str) -> str:
        """The JSON name a result path IRI is the expansion of."""
        for term, definition in self.context.items():
            target = definition.get("@id") if isinstance(definition, dict) else definition
            if isinstance(target, str) and (target == iri or self._expand(target) == iri):
                return term
        return iri

    @staticmethod
    def _first(graph, path):
        """A sequence path (a list member's shape) by the attribute it starts at."""
        from rdflib import BNode
        from rdflib.collection import Collection

        return Collection(graph, path)[0] if isinstance(path, BNode) else path

    def _expand(self, curie: str) -> str:
        prefix, _, local = curie.partition(":")
        base = self.context.get(prefix)
        return base + local if isinstance(base, str) and local and not local.startswith("//") else curie

    def verdict(self, record: dict[str, Any]) -> Verdict:
        import jsonschema
        from pyshacl import validate
        from rdflib import Namespace

        sh = Namespace("http://www.w3.org/ns/shacl#")
        from rdflib import URIRef

        # Only the record is the focus: the referenced entities are stand-ins with a type alone.
        conforms, results, _ = validate(
            self._graph(record), shacl_graph=self.shapes, advanced=True, focus_nodes=[URIRef(record["id"])]
        )
        paths = sorted({self._term(str(self._first(results, path))) for path in results.objects(None, sh.resultPath)})
        against = dict(self.schema)
        against.pop("additionalProperties", None)
        against["$ref"] = f"#/definitions/{record['type']}"
        schema_ok = not any(True for _ in jsonschema.Draft7Validator(against).iter_errors(record))
        return Verdict(bool(conforms), paths, schema_ok)


def check(source: str | Path) -> list[str]:
    """Every disagreement between the datasets, SHACL and the JSON Schema of one model."""
    sets = compile_datasets(source)
    checker = Checker(source)
    problems: list[str] = []
    for record in sets.valid:
        got = checker.verdict(record)
        if not got.shacl_ok:
            problems.append(f"valid {record['type']} refused by SHACL at {', '.join(got.shacl_paths) or '(no path)'}")
        if not got.schema_ok:
            problems.append(f"valid {record['type']} refused by the JSON Schema")
    for bad in sets.invalid:
        got = checker.verdict(bad.record)
        what = f"{bad.record['type']}.{bad.path} ({bad.constraint})"
        if got.shacl_ok:
            problems.append(f"invalid {what} accepted by SHACL")
        elif bad.constraint != "closed" and bad.path not in got.shacl_paths:
            problems.append(f"invalid {what} refused by SHACL at {', '.join(got.shacl_paths)}, not at {bad.path}")
        if got.schema_ok:
            problems.append(f"invalid {what} accepted by the JSON Schema")
    return problems


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("sources", nargs="+", help="LinkML YAML files")
    parser.add_argument(
        "-o", "--output", help="write <model>.valid.json, <model>.invalid.json and <model>.schema.json here"
    )
    args = parser.parse_args(argv)

    failed = False
    for source in args.sources:
        try:
            sets = compile_datasets(source)
            problems = check(source)
        except ModelError as err:
            print(f"{source}: error: {err}", file=sys.stderr)
            failed = True
            continue
        print(f"{source}: {len(sets.valid)} valid, {len(sets.invalid)} invalid, {len(problems)} disagreements")
        for problem in problems:
            print(f"  {problem}")
        failed = failed or bool(problems)
        if args.output:
            stem = Path(source).name.removesuffix(".linkml.yaml")
            out = Path(args.output)
            out.mkdir(parents=True, exist_ok=True)
            (out / f"{stem}.valid.json").write_text(json.dumps(sets.valid, indent=2, sort_keys=True) + "\n")
            invalid = [{"path": i.path, "constraint": i.constraint, "record": i.record} for i in sets.invalid]
            (out / f"{stem}.invalid.json").write_text(json.dumps(invalid, indent=2, sort_keys=True) + "\n")
            # The JSON Schema the gateway compiles its write check from (jc-manifest model_schema).
            schema = compile_schema(source)
            (out / f"{stem}.schema.json").write_text(json.dumps(schema, indent=2, sort_keys=True) + "\n")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
