"""Smart Data Models → LinkML (T-0171, DM-07…DM-11, DM-18).

The wizard hands over a catalogue identifier, `dataModel.Environment/AirQualityObserved`, and
gets a LinkML model back. Two halves, deliberately separate:

* `fetch` is the only code in Model Tools that opens a socket. It builds every URL itself from
  a validated identifier and refuses anything that does not land on the Smart Data Models
  organisation, so no caller can steer the fetch (DM-10).
* `convert` is a pure function over the three fetched documents. It is what the tests drive,
  and what CI re-runs to prove an import is reproducible from the recorded commit (DM-08).
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from tempfile import NamedTemporaryFile
from typing import Any

import requests
import yaml

from common import UPSTREAM_ANNOTATION

SDM_ORG = "smart-data-models"
SDM_NAMESPACE = "https://smartdatamodels.org/"
RAW_BASE = f"https://raw.githubusercontent.com/{SDM_ORG}/"
API_BASE = f"https://api.github.com/repos/{SDM_ORG}/"

#: Every URL Model Tools may open. A URL that does not start with one of these is refused
#: before a socket exists, whatever produced it (DM-10, DM-18).
ALLOWED_PREFIXES = (RAW_BASE, API_BASE)

#: The documents an import needs, at the paths the catalogue publishes them under. The schema
#: and the example are per model; the `@context` is **one per subject repository, at its
#: root**, because every model of a subject shares the same term definitions. Fetching it as
#: `{model}/context.jsonld` answers 404 for every model in the catalogue.
SCHEMA_FILE = "schema.json"
CONTEXT_FILE = "context.jsonld"
EXAMPLE_FILE = "examples/example-normalized.jsonld"

#: The shared commons, one document for the whole catalogue: every schema `$ref`s it for the
#: attributes every entity has (`name`, `owner`, `dataProvider`, `address`, …). The catalogue
#: writes that reference against the organisation's GitHub Pages host, which the DM-10
#: allowlist does not cover — and does not need to, because the same file is published in the
#: `data-models` repository, which the allowlist already covers. Widening the allowlist to a
#: second host would buy nothing and cost a host.
COMMONS_REPO = "data-models"
COMMONS_FILE = "common-schema.json"

#: What a `$ref` into that document looks like, whichever host it names.
COMMONS_REF = re.compile(r"/data-models/common-schema\.json#(.+)$")

#: How the catalogue writes the NGSI-LD kind: as the first word of the description, followed
#: by a full stop. Nothing else in the schema says it — Smart Data Models annotates no kinds
#: and its `@context` is a flat term-to-IRI map — so `refDevice` and every other reference
#: imports as a Property, which is a `@context` a consumer cannot follow (DM-05).
KIND_IN_DESCRIPTION = re.compile(
    r"^\s*(Property|Relationship|GeoProperty|LanguageProperty|ListProperty|JsonProperty"
    r"|VocabProperty)\s*\."
)

#: How deep a chain of `$ref`s is followed before it is treated as a loop.
MAX_REF_DEPTH = 12

#: A catalogue identifier: `dataModel.<Subject>/<Model>`. The Portal already refuses anything
#: else (API/01 §11); Model Tools does not trust that it is the only caller.
IDENTIFIER = re.compile(r"^(?!\.)[A-Za-z0-9._-]{1,128}/(?!\.)[A-Za-z0-9._-]{1,128}$")

#: One path segment of a catalogue URL: a repository name, a model name or a ref. No slash and
#: no leading dot, so a segment can neither add a path of its own nor start a traversal.
SEGMENT = re.compile(r"^(?!\.)[A-Za-z0-9._-]{1,128}$")

#: Attributes that belong to every NGSI-LD entity and therefore to the shared import, not to
#: the imported model (DM-09).
CORE_SLOTS = ("id", "type", "location", "observedAt")

#: Everything `linkml:types` defines, which is every range an imported model may name without
#: declaring it. LinkML refuses to load a schema whose slot has an unrecognized range, so a
#: range outside this set has to be an enum or a class the document itself carries.
LINKML_TYPES = frozenset(
    {
        "boolean", "curie", "date", "date_or_datetime", "datetime", "decimal", "double",
        "float", "integer", "jsonpath", "jsonpointer", "ncname", "nodeidentifier",
        "objectidentifier", "sparqlpath", "string", "time", "uri", "uriorcurie",
    }
)

REQUEST_TIMEOUT = 20


class ImportError_(Exception):
    """An import that cannot proceed. The message reaches the person running the wizard."""


def split_identifier(model: str) -> tuple[str, str]:
    """`dataModel.Environment/AirQualityObserved` → (subject repository, model name)."""
    if not IDENTIFIER.match(model):
        raise ImportError_(
            f"'{model}' is not a Smart Data Models identifier "
            "(expected 'dataModel.<Subject>/<Model>'); a URL is never accepted"
        )
    subject, name = model.split("/", 1)
    return subject, name


def _segment(value: str, what: str) -> str:
    """One URL path segment, checked before it is put into a URL."""
    if not SEGMENT.match(value):
        raise ImportError_(f"'{value}' is not a usable {what} for a Smart Data Models URL")
    return value


def _get(url: str) -> requests.Response:
    """One HTTP GET, and only to the Smart Data Models organisation (DM-10).

    The prefix alone is not the check: `…/smart-data-models/../other-org/x` starts with the
    allowed prefix and still leaves the organisation once a server normalises it, so a URL
    carrying a traversal is refused as well.
    """
    if not url.startswith(ALLOWED_PREFIXES) or ".." in url:
        raise ImportError_(f"refusing to fetch outside the Smart Data Models allowlist: {url}")
    response = requests.get(url, timeout=REQUEST_TIMEOUT)
    if response.status_code != 200:
        raise ImportError_(f"{url} answered {response.status_code}")
    return response


def resolve_commit(subject: str, ref: str = "master") -> str:
    """The commit an import is pinned to, so a re-import is reproducible (DM-08).

    A ref is one segment: the catalogue's branches are plain names, and accepting a slash here
    would let a ref carry a path of its own.
    """
    url = f"{API_BASE}{_segment(subject, 'repository')}/commits/{_segment(ref, 'ref')}"
    return _get(url).json()["sha"]


def _pointer(document: Any, pointer: str) -> Any:
    """What a JSON pointer names inside `document`, or None."""
    node = document
    for token in pointer.strip("/").split("/"):
        token = token.replace("~1", "/").replace("~0", "~")
        if not isinstance(node, dict) or token not in node:
            return None
        node = node[token]
    return node


def resolve(node: Any, commons: dict[str, Any] | None, _depth: int = 0) -> Any:
    """`node` with every `$ref` into the shared commons replaced by what it points at.

    A catalogue schema is an `allOf` of the commons and one inline branch, and single
    attributes are references of their own (`address` and `location` both are). Leaving them
    unresolved imports a model missing exactly the attributes a federation partner sends, and
    keeping the ones it does import shapeless (DM-11).

    This resolves references into one known document and nothing else: a `$ref` naming
    anything but the commons is left exactly as it is, so no reference can turn into a fetch.
    """
    if not commons or _depth > MAX_REF_DEPTH:
        return node
    if isinstance(node, list):
        return [resolve(item, commons, _depth + 1) for item in node]
    if not isinstance(node, dict):
        return node
    reference = node.get("$ref")
    match = COMMONS_REF.search(reference) if isinstance(reference, str) else None
    if match:
        # Siblings of a `$ref` are how the catalogue narrows what it points at, so they win.
        local = {key: value for key, value in node.items() if key != "$ref"}
        target = _pointer(commons, match.group(1))
        if isinstance(target, dict):
            return resolve({**target, **local}, commons, _depth + 1)
        return resolve(local, commons, _depth + 1)
    return {key: resolve(value, commons, _depth + 1) for key, value in node.items()}


def fetch(model: str, ref: str = "master") -> dict[str, Any]:
    """Fetch the documents one import needs, pinned to a commit."""
    subject, name = split_identifier(model)
    commit = resolve_commit(subject, ref)

    def document(filename: str, *, of_the_model: bool = True) -> Any:
        under = f"{name}/" if of_the_model else ""
        return _get(f"{RAW_BASE}{subject}/{commit}/{under}{filename}").json()

    # The commons live in their own repository on their own branch, so they carry a commit of
    # their own. It is recorded: DM-08 pins every fetched artifact, and an import whose commons
    # are whatever `master` held that afternoon is not reproducible. A failure here is not
    # survivable — a model quietly missing `name`, `owner` and `dataProvider` is the defect
    # this fetch exists to fix — so it is left to raise.
    commons_commit = resolve_commit(COMMONS_REPO)
    commons = _get(f"{RAW_BASE}{COMMONS_REPO}/{commons_commit}/{COMMONS_FILE}").json()

    return {
        "schema": document(SCHEMA_FILE),
        "context": document(CONTEXT_FILE, of_the_model=False),
        "commons": commons,
        # A model without a published example is still importable; the example only seeds the
        # editor's preview and the golden test of a later Mapping.
        "example": _example(subject, name, commit),
        "provenance": {
            "repository": f"https://github.com/{SDM_ORG}/{subject}",
            "path": f"{name}/{SCHEMA_FILE}",
            "commit": commit,
            "commons": f"https://github.com/{SDM_ORG}/{COMMONS_REPO}@{commons_commit}",
        },
    }


def _example(subject: str, name: str, commit: str) -> Any | None:
    try:
        return _get(f"{RAW_BASE}{subject}/{commit}/{name}/{EXAMPLE_FILE}").json()
    except ImportError_:
        return None


def _term_kind(term: Any) -> str | None:
    """The NGSI-LD kind a `@context` term implies (DM-05).

    Smart Data Models does not annotate kinds, but its `@context` already says what a value
    is: an `@id` type is a relationship, a language container is a language map.
    """
    if not isinstance(term, dict):
        return None
    if term.get("@type") == "@id":
        return "Relationship"
    if term.get("@container") == "@language":
        return "LanguageProperty"
    return None


def _structural_kind(slot_name: str, definition: dict[str, Any]) -> str:
    """The kind the shape of the value implies, which is the only one that cannot be wrong."""
    if slot_name == "location" or (definition.get("format") or "") == "geojson":
        return "GeoProperty"
    if definition.get("type") == "object":
        return "JsonProperty"
    if definition.get("type") == "array":
        return "ListProperty"
    return "Property"


def _described_kind(definition: dict[str, Any]) -> str | None:
    """The kind the description names, by the convention the whole catalogue follows.

    `"Relationship. A reference to the device(s) which captured this observation"`. Reading a
    kind out of prose is a heuristic, so it is a narrow one: the whole first word, one of the
    seven kinds, a full stop after it. Anything else — a description starting with a sentence,
    a kind that is not a kind, no description at all — leaves the answer to the shape.
    """
    match = KIND_IN_DESCRIPTION.match(str(definition.get("description") or ""))
    return match.group(1) if match else None


def _kind_of(slot_name: str, definition: dict[str, Any], context: dict[str, Any]) -> str:
    """The NGSI-LD kind of one attribute, from the three places that can say so (DM-05).

    The `@context` first, because a term typed `@id` is a statement and not a guess. Then the
    shape, which catches the geo and object attributes the catalogue describes as plain
    "Property." anyway. The description is consulted last and only when the shape says
    nothing: it is the only place a Relationship is written, and the only one that can lie.
    """
    from_context = _term_kind(context.get(slot_name))
    if from_context:
        return from_context
    structural = _structural_kind(slot_name, definition)
    if structural != "Property":
        return structural
    return _described_kind(definition) or "Property"


#: JSON Schema string formats whose LinkML range schema-automator leaves at the default.
#: A timestamp typed as a string is a timestamp nothing can filter on (DM-20).
FORMAT_RANGES = {"date-time": "datetime", "date": "date", "time": "time", "uri": "uriorcurie"}


def _range_of(definition: dict[str, Any], imported: dict[str, Any]) -> str | None:
    return imported.get("range") or FORMAT_RANGES.get(definition.get("format") or "")


def _iri(slot_name: str, context: dict[str, Any]) -> str | None:
    """The canonical Smart Data Models IRI of a term, from the upstream `@context` (DM-09)."""
    term = context.get(slot_name)
    if isinstance(term, str):
        return term
    if isinstance(term, dict) and isinstance(term.get("@id"), str):
        return term["@id"]
    return None


def _composed(schema_json: dict[str, Any], key: str) -> dict[str, Any] | list[Any]:
    """`properties` or `required` of a catalogue schema, whichever branch declares them.

    A Smart Data Models schema declares almost nothing at its top level: it is an `allOf` of
    the shared commons, by `$ref`, and one inline branch carrying the model's own attributes.
    Reading only the top level imports a model with no slots at all.

    The `$ref`ed commons branch is a plain object by the time this runs: `resolve` has already
    replaced it with the document `fetch` pulled from the `data-models` repository.
    """
    merged: dict[str, Any] = {}
    order: list[Any] = []
    for branch in [*(schema_json.get("allOf") or []), schema_json]:
        if not isinstance(branch, dict):
            continue
        value = branch.get(key)
        if isinstance(value, dict):
            merged.update(value)
        elif isinstance(value, list):
            order.extend(item for item in value if item not in order)
    return order if key == "required" else merged


def _flattened(schema_json: dict[str, Any]) -> dict[str, Any]:
    """The catalogue schema as one object, with the `allOf` branches merged into the top.

    schema-automator derives the ranges and the enums, and it reads the top level too, so a
    schema handed over unflattened produces a model with no slots, no ranges and no enums.
    """
    flat = {key: value for key, value in schema_json.items() if key != "allOf"}
    flat["properties"] = _composed(schema_json, "properties")
    flat["required"] = _composed(schema_json, "required")
    return flat


def _automator_schema(schema_json: dict[str, Any], name: str) -> dict[str, Any]:
    """schema-automator's LinkML output for one JSON Schema, as plain data (DM-09)."""
    from linkml_runtime.dumpers import yaml_dumper
    from schema_automator.importers.jsonschema_import_engine import JsonSchemaImportEngine

    with NamedTemporaryFile("w", suffix=".json", delete=False, encoding="utf-8") as handle:
        json.dump(schema_json, handle)
        path = handle.name
    try:
        schema = JsonSchemaImportEngine().load(path, name=name, format="json")
    finally:
        Path(path).unlink(missing_ok=True)
    return yaml.safe_load(yaml_dumper.dumps(schema))


def _class_name(slot_name: str, imported: dict[str, Any], taken: set[str], prefix: str) -> str:
    """A name for the class a nested object becomes, that collides with nothing already used."""
    candidate = imported.get("range") or slot_name[:1].upper() + slot_name[1:]
    if candidate in taken:
        candidate = f"{prefix}{candidate[:1].upper()}{candidate[1:]}"
    while candidate in taken:
        candidate += "_"
    return candidate


def _nested_classes(
    slot_name: str,
    definition: dict[str, Any],
    imported: dict[str, Any],
    taken: set[str],
    prefix: str,
    classes: dict[str, Any],
    upstream: str,
) -> str | None:
    """The class one nested object becomes, and its own nested objects with it.

    schema-automator answers `address: {range: Address}` and then defines no class `Address`,
    and LinkML refuses to load a schema whose slot names a range it cannot resolve — so the
    range had to be dropped, and the shape of the object went with it. The shape is in the
    JSON Schema all along, under the attribute's own `properties`. `attributes` rather than
    top-level slots, because a nested `name` and the entity's `name` are two different things
    and class-local attributes cannot collide.
    """
    properties = definition.get("properties")
    if definition.get("type") != "object" or not isinstance(properties, dict) or not properties:
        return None

    class_name = _class_name(slot_name, imported, taken, prefix)
    taken.add(class_name)
    required = definition.get("required") or []
    attributes: dict[str, Any] = {}
    for attribute_name, sub in properties.items():
        if not isinstance(sub, dict):
            continue
        nested = _nested_classes(
            attribute_name, sub, {}, taken, class_name, classes, upstream
        )
        attribute = {
            "description": sub.get("description"),
            "range": nested or FORMAT_RANGES.get(sub.get("format") or ""),
            "required": attribute_name in required,
            "annotations": {
                "ngsi_ld_kind": _kind_of(attribute_name, sub, {}),
                # A nested attribute is an upstream term like any other, and the citation is
                # what keeps it out of the squatting guard: without it the generator refuses
                # the model for minting `streetAddress` under the catalogue's namespace.
                UPSTREAM_ANNOTATION: upstream,
            },
        }
        attributes[attribute_name] = {
            key: value for key, value in attribute.items() if value is not None and value is not False
        }

    classes[class_name] = {
        "description": definition.get("description"),
        "attributes": attributes,
        "annotations": {UPSTREAM_ANNOTATION: upstream},
    }
    classes[class_name] = {k: v for k, v in classes[class_name].items() if v is not None}
    return class_name


def convert(
    model: str,
    schema_json: dict[str, Any],
    context_jsonld: dict[str, Any],
    example: Any | None,
    provenance: dict[str, str],
    commons: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Turn the fetched documents into one LinkML model.

    Every upstream slot is kept (DM-11): a slot nobody uses locally is still what a federation
    partner sends, and a model that quietly drops it starts rejecting valid payloads.
    """
    _, name = split_identifier(model)
    context = context_jsonld.get("@context", {}) if isinstance(context_jsonld, dict) else {}
    if not isinstance(context, dict):
        raise ImportError_("the fetched context.jsonld has no object under @context")
    resolved = resolve(schema_json, commons) if isinstance(schema_json, dict) else {}
    flat = _flattened(resolved) if isinstance(resolved, dict) else {}
    automator = _automator_schema(flat, name)
    properties = flat.get("properties") or {}
    required = flat.get("required") or []
    upstream = f"{provenance['repository']}@{provenance['commit']}"

    slots: dict[str, Any] = {}
    nested: dict[str, Any] = {}
    taken = {name} | set(automator.get("enums") or {})
    for slot_name, definition in properties.items():
        if slot_name in CORE_SLOTS:
            # Declared once in ngsi-ld-core and inherited, so every model agrees on them.
            continue
        if not isinstance(definition, dict):
            continue
        imported = (automator.get("slots") or {}).get(slot_name, {})
        shape = _nested_classes(slot_name, definition, imported, taken, name, nested, upstream)
        slot: dict[str, Any] = {
            "description": definition.get("description") or imported.get("description"),
            "range": shape or _range_of(definition, imported),
            "required": slot_name in required,
            "annotations": {
                "ngsi_ld_kind": _kind_of(slot_name, definition, context),
                # The IRI below is upstream's; this is the citation that says so (DM-04).
                UPSTREAM_ANNOTATION: upstream,
            },
        }
        iri = _iri(slot_name, context)
        if iri:
            slot["slot_uri"] = iri
        slots[slot_name] = {k: v for k, v in slot.items() if v is not None and v is not False}

    # Enums schema-automator derived for a core slot (`type`) have no slot left to serve.
    referenced = {slot.get("range") for slot in slots.values()}
    enums = {k: v for k, v in (automator.get("enums") or {}).items() if k in referenced}

    # A range LinkML cannot resolve makes it refuse the whole document, so a range that names
    # neither a built-in type, nor an enum, nor a class this import defines is dropped and the
    # attribute falls back to the default range. With the nested shapes above that is now rare
    # rather than the normal case for every object-valued attribute.
    resolvable = LINKML_TYPES | set(enums) | set(nested) | {name}
    for slot in slots.values():
        if slot.get("range") and slot["range"] not in resolvable:
            del slot["range"]

    # Without a prefix every IRI is written out in full and every generator warns about it;
    # the upstream namespaces are known here, so they are declared once.
    prefixes = {"linkml": "https://w3id.org/linkml/"}
    if any(str(slot.get("slot_uri", "")).startswith(SDM_NAMESPACE) for slot in slots.values()):
        prefixes["sdm"] = SDM_NAMESPACE

    model_iri = _iri(name, context) or f"{provenance['repository']}#{name}"
    document: dict[str, Any] = {
        "id": f"{provenance['repository']}/{name}",
        "name": name,
        "title": schema_json.get("title") or name,
        "description": schema_json.get("description"),
        "prefixes": prefixes,
        "default_range": "string",
        "imports": ["linkml:types", "ngsi-ld-core"],
        "annotations": {
            "spec.source.repository": provenance["repository"],
            "spec.source.path": provenance["path"],
            "spec.source.commit": provenance["commit"],
        },
        "classes": {
            name: {
                "class_uri": model_iri,
                "description": schema_json.get("description"),
                "is_a": "Entity",
                "slots": sorted(slots),
                "annotations": {UPSTREAM_ANNOTATION: upstream},
            },
            **nested,
        },
        "slots": slots,
        # The enums schema-automator derived from `enum` keywords carry the permissible
        # values the gateway validates against, so they travel with the model.
        "enums": enums,
    }
    # The commons are a fifth document, in a repository of their own, so they carry a commit
    # of their own; DM-08 pins every fetched artifact and this is one. A conversion driven
    # from documents on disk has no commons commit, and says nothing rather than "None".
    if provenance.get("commons"):
        document["annotations"]["spec.source.commons"] = provenance["commons"]
    if example is not None:
        document["annotations"]["spec.source.example"] = json.dumps(example, sort_keys=True)
    return {k: v for k, v in document.items() if v not in (None, {}, [])}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("model", help="catalogue identifier, dataModel.Environment/AirQualityObserved")
    parser.add_argument("--ref", default="master", help="upstream branch or tag to pin from")
    parser.add_argument("-o", "--output", help="write here instead of stdout")
    args = parser.parse_args(argv)

    try:
        fetched = fetch(args.model, args.ref)
        document = convert(
            args.model,
            fetched["schema"],
            fetched["context"],
            fetched["example"],
            fetched["provenance"],
            fetched.get("commons"),
        )
    except (ImportError_, requests.RequestException) as err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    text = yaml.safe_dump(document, sort_keys=False, allow_unicode=True)
    if args.output:
        Path(args.output).write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
