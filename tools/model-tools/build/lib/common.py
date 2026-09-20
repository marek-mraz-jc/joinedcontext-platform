"""Pieces every Model Tools generator needs (DM-18).

Model Tools is a pure function: it reads a LinkML document, renders artifacts and writes
nothing. It holds no credentials, reads no platform state and reaches no network except the
Smart Data Models allowlist of `import_sdm` (DM-10). Everything here obeys that.
"""

from __future__ import annotations

import importlib.metadata as metadata
import os
from contextlib import contextmanager
from pathlib import Path
from tempfile import NamedTemporaryFile
from typing import Any, Iterator

import jsonasobj2
from linkml_runtime import SchemaView
from linkml_runtime.linkml_model.meta import ClassDefinition, SlotDefinition

#: The NGSI-LD kinds a slot may declare (DM-05). A slot without the annotation is a Property:
#: the plain attribute is the common case and writing it out on every slot is noise.
NGSI_LD_KINDS = (
    "Property",
    "GeoProperty",
    "Relationship",
    "LanguageProperty",
    "ListProperty",
    "JsonProperty",
    "VocabProperty",
)

DEFAULT_KIND = "Property"

#: Namespaces an organisation must never mint its own terms under (DM-04, DM-16). Reusing an
#: upstream IRI is how a model claims to mean the same thing as a standard; minting a *new*
#: term under someone else's namespace is squatting, and it breaks every consumer that
#: resolves the IRI.
RESERVED_NAMESPACES = (
    "https://smartdatamodels.org/",
    "https://raw.githubusercontent.com/smart-data-models/",
    "https://github.com/smart-data-models/",
    "https://uri.etsi.org/",
    "http://uri.etsi.org/",
    "https://www.w3.org/",
    "http://www.w3.org/",
    "https://w3id.org/linkml/",
)

#: A slot that carries this annotation came from an upstream catalogue with its IRI, so a
#: reserved namespace on it is a citation and not a claim. `import_sdm` sets it; a hand-written
#: model cannot earn one without saying where the term comes from.
UPSTREAM_ANNOTATION = "upstream_source"

#: A slot that is a JSON-LD keyword rather than a term: `id` is the node's own IRI and `type`
#: is `rdf:type`. They are slots in LinkML because an NGSI-LD payload carries them as JSON
#: members, and they are keywords everywhere the payload is read as RDF.
JSONLD_KEYWORD_ANNOTATION = "jsonld_keyword"


#: The shared imports Model Tools ships. A model writes `imports: [ngsi-ld-core]` and gets
#: `id`, `type`, `location` and `observedAt` (DM-09); resolving it from the image is what keeps
#: generation working with no network at all (DM-18). The folder sits beside `src/` in the
#: repository and is copied to its own path in the image, which is what `MODEL_TOOLS_MODELS`
#: names: an installed module has no repository around it to walk up into.
# Absolute on purpose: the import map is handed to LinkML's loader, which resolves a relative
# entry against the *schema's* directory and not the working directory, so a relative
# MODEL_TOOLS_MODELS would look for the shipped models beside every model it is asked to read.
SHIPPED_MODELS = Path(
    os.environ.get("MODEL_TOOLS_MODELS") or Path(__file__).resolve().parent.parent / "models"
).resolve()
# The loader appends `.yaml` to whatever an import maps to, so the entry stops at the stem.
IMPORT_MAP = {"ngsi-ld-core": str(SHIPPED_MODELS / "ngsi-ld-core.linkml")}


class ModelError(Exception):
    """A model the generators refuse. The message is shown to the person editing it."""


@contextmanager
def as_path(source: str | Path) -> Iterator[str]:
    """A file path for a schema given either as a path or as the YAML text itself.

    The editor holds the document in memory and has no file to point at, while CI has a path
    and no reason to read it twice; `SchemaView` accepts a path, so text is spooled to a
    temporary file that is removed as soon as the caller is done with it. The service renders
    four artifacts from one source and spools it once, which is also what makes the four
    report the same parse error rather than four paths' worth of the same one.
    """
    text = str(source)
    if "\n" not in text and Path(text).exists():
        yield text
        return
    with NamedTemporaryFile("w", suffix=".yaml", delete=False, encoding="utf-8") as handle:
        handle.write(text)
        path = handle.name
    try:
        yield path
    finally:
        Path(path).unlink(missing_ok=True)


def load(source: str | Path) -> SchemaView:
    """Read a LinkML schema from a file path or from the YAML text itself."""
    with as_path(source) as path:
        return _view(path)


def _view(path: str) -> SchemaView:
    """A view whose imports are already merged in.

    Every LinkML generator builds its own `SchemaView` from the schema it is handed, and that
    one has no import map, so an unmerged schema loses `ngsi-ld-core` the moment it reaches a
    generator. Merging once here is also what makes the artifacts self-contained: a consumer
    of the JSON Schema or the SHACL shapes has no way to resolve our imports.
    """
    view = SchemaView(path, importmap=IMPORT_MAP)
    view.merge_imports()
    return view


def generator_version() -> str:
    """The version that produced an artifact set (DM-19, DM-43).

    CI and the Portal preview must run the same Model Tools version, and the only way to
    compare a committed artifact with a preview is for both to say what rendered them.
    """
    return f"linkml-{metadata.version('linkml')}"


def annotation_value(element: SlotDefinition | ClassDefinition, tag: str) -> str | None:
    """One annotation of a slot or class, or None where it carries none.

    An induced slot carries plain `JsonObj` annotations while a declared one carries
    `Annotation` objects, and the generators read both, so the two shapes are flattened here
    instead of at every call site.
    """
    annotations = getattr(element, "annotations", None)
    if not annotations:
        return None
    entry = jsonasobj2.as_dict(annotations).get(tag)
    if entry is None:
        return None
    value = entry["value"] if isinstance(entry, dict) else entry.value
    return None if value is None else str(value)


def ngsi_ld_kind(slot: SlotDefinition) -> str:
    """The declared NGSI-LD kind of a slot (DM-05)."""
    value = annotation_value(slot, "ngsi_ld_kind")
    if value is None:
        return DEFAULT_KIND
    if value not in NGSI_LD_KINDS:
        raise ModelError(
            f"slot '{slot.name}' declares ngsi_ld_kind '{value}', "
            f"which is not one of {', '.join(NGSI_LD_KINDS)}"
        )
    return value


def reserved_namespace(iri: str) -> str | None:
    """The reserved namespace an IRI falls under, or None where it is the organisation's own."""
    return next((ns for ns in RESERVED_NAMESPACES if iri.startswith(ns)), None)


def unit_of(slot: SlotDefinition) -> dict[str, Any] | None:
    """The UN/CEFACT unit of a slot as plain JSON (DM-06).

    Exports put the unit in the CSV/XLSX header and dashboards put it on the axis, so it has
    to survive into the generated artifacts rather than staying in the LinkML source.
    """
    unit = getattr(slot, "unit", None)
    if unit is None:
        return None
    fields = {
        "ucumCode": getattr(unit, "ucum_code", None),
        "symbol": getattr(unit, "symbol", None),
        "descriptiveName": getattr(unit, "descriptive_name", None),
        # The CEFACT common code travels in exact_mappings, per DM-06.
        "exactMappings": list(getattr(unit, "exact_mappings", None) or []),
    }
    present = {k: v for k, v in fields.items() if v}
    return present or None


def slots_of(view: SchemaView, cls: ClassDefinition) -> list[SlotDefinition]:
    """Every slot of a class, induced so inherited and imported slots are included."""
    return [view.induced_slot(name, cls.name) for name in view.class_slots(cls.name)]
