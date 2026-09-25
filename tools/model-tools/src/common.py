"""Pieces every Model Tools generator needs (DM-18).

Model Tools is a pure function: it reads a LinkML document, renders artifacts and writes
nothing. It holds no credentials, reads no platform state and reaches no network except the
Smart Data Models allowlist of `import_sdm` (DM-10). Everything here obeys that.
"""

from __future__ import annotations

import importlib.metadata as metadata
import os
import re
from contextlib import contextmanager
from contextvars import ContextVar
from pathlib import Path
from tempfile import NamedTemporaryFile, TemporaryDirectory
from typing import Any, Iterator

import jsonasobj2
import yaml
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

#: A platform model an import names at a pinned major (DM-75): an organization model, or a model
#: of the importing model's own project. No colon, so LinkML never expands it as a CURIE; no
#: slash, so LinkML never resolves the imported model's own imports relative to its name.
MODEL_IMPORT = re.compile(r"^(org|project)\.[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?\.v(0|[1-9][0-9]*)$")

#: The platform models the current request imports, name to spooled stem. Model Tools reads no
#: platform state, so the caller resolves them and hands over their sources; a context
#: variable keeps two requests compiling at once from seeing each other's (DM-18).
_REQUEST_IMPORTS: ContextVar[dict[str, str]] = ContextVar("request_imports", default={})


@contextmanager
def imported(sources: dict[str, str]) -> Iterator[None]:
    """Make `sources` (import name to LinkML text) resolvable for the calls inside the block.

    Each source is spooled under a name of our own, never the import name, so nothing the
    caller sends picks a path; the folder is removed when the block ends.
    """
    with TemporaryDirectory() as folder:
        mapping: dict[str, str] = {}
        for index, (name, text) in enumerate(sorted(sources.items())):
            stem = Path(folder) / f"import-{index}"
            stem.with_suffix(".yaml").write_text(text, encoding="utf-8")
            mapping[name] = str(stem)
        token = _REQUEST_IMPORTS.set(mapping)
        try:
            yield
        finally:
            _REQUEST_IMPORTS.reset(token)


def model_imports(text: str) -> list[str]:
    """The platform-model imports one LinkML document names, and every other `org.` or
    `project.` entry, malformed ones included: those are refused rather than read as a file."""
    try:
        document = yaml.safe_load(text)
    except yaml.YAMLError:
        return []
    imports = document.get("imports") if isinstance(document, dict) else None
    if not isinstance(imports, list):
        return []
    return [entry for entry in imports if isinstance(entry, str) and entry.split(".", 1)[0] in ("org", "project") and "." in entry]


class ModelError(Exception):
    """A model the generators refuse. The message is shown to the person editing it."""


class RelationshipError(ModelError):
    """A model whose relationships break a rule of DM-68, one entry per broken rule.

    Each entry is `{path}: {rule}: {message}`: `/generate` answers them one per error, which is
    what the Portal's save route refuses with (DM-68, CC-24).
    """

    def __init__(self, problems: list[str]) -> None:
        super().__init__("; ".join(problems))
        self.problems = problems


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
        view = _view(path)
    check_unit_prefixes(view)
    check_relationships(view)
    check_data_structures(view)
    return view


def check_relationships(view: SchemaView) -> None:
    """Refuse a model with a broken relationship, every rule at once (DM-68)."""
    # Imported here: `relationships` reads this module's helpers.
    from relationships import analyse

    _, problems = analyse(view)
    if problems:
        raise RelationshipError(problems)


def check_unit_prefixes(view: SchemaView) -> None:
    """Refuse a unit mapping whose prefix the model never declared (DM-59).

    A unit says two things: the UN/CEFACT code NGSI-LD puts on the wire, and the QUDT IRI a
    federated reader resolves to align two organisations' measurements. Both travel as CURIEs
    in `exact_mappings` and `has_quantity_kind`, and a CURIE whose prefix is not in the model
    is a dangling string in every artifact that carries it — the JSON Schema's `x-unit`, the
    documentation table, the RDF. It is caught here rather than by whoever dereferences it.
    """
    declared = set(view.schema.prefixes or {})
    dangling: list[str] = []
    for slot in view.all_slots().values():
        unit = getattr(slot, "unit", None)
        if unit is None:
            continue
        curies = list(getattr(unit, "exact_mappings", None) or [])
        kind = getattr(unit, "has_quantity_kind", None)
        if kind:
            curies.append(str(kind))
        for curie in curies:
            # An absolute IRI needs no prefix; a CURIE is `prefix:reference`.
            if "://" in curie or ":" not in curie:
                continue
            prefix = curie.split(":", 1)[0]
            if prefix not in declared:
                dangling.append(f"'{slot.name}' → {curie}")
    if dangling:
        raise ModelError(
            "these unit mappings use a prefix the model does not declare, so the CURIE "
            "resolves to nothing wherever the artifacts carry it; add it to `prefixes`: "
            + "; ".join(sorted(dangling))
        )


#: The class annotation that makes a class a Data Structure Definition (DM-60).
QB_DSD_ANNOTATION = "qb_dsd"
#: The slot annotation that names a slot's role in the cube (DM-60).
QB_COMPONENT_ANNOTATION = "qb_component"
QB_COMPONENTS = ("dimension", "measure")
#: The ranges the one time dimension may have: a period is a date, an instant or a year.
QB_TIME_RANGES = ("date", "datetime", "integer")


def is_dsd(cls: ClassDefinition) -> bool:
    """Whether the class is annotated as a Data Structure Definition (DM-60)."""
    return (annotation_value(cls, QB_DSD_ANNOTATION) or "").lower() == "true"


def component_of(slot: SlotDefinition) -> str | None:
    """The cube role a slot declares, `dimension` or `measure`, or None."""
    return annotation_value(slot, QB_COMPONENT_ANNOTATION)


def dsd_components(view: SchemaView, cls: ClassDefinition) -> list[tuple[SlotDefinition, str]]:
    """The components of a DSD class in slot order, each with its role."""
    return [(slot, role) for slot in slots_of(view, cls) if (role := component_of(slot)) is not None]


def check_data_structures(view: SchemaView) -> None:
    """Refuse a Data Structure Definition that breaks a rule of DM-60, every rule at once.

    A cube whose observation may leave out a dimension is not a table, and a measure without a
    unit is a number nobody can compare; both are refused here rather than discovered by the
    first statistician who reads `model.qb.ttl`.
    """
    problems: list[str] = []
    dsd_slots: set[str] = set()
    for cls in view.all_classes().values():
        flag = annotation_value(cls, QB_DSD_ANNOTATION)
        if flag is not None and flag.lower() not in ("true", "false"):
            problems.append(f"class '{cls.name}' has {QB_DSD_ANNOTATION} '{flag}'; it is true or false")
            continue
        if not is_dsd(cls):
            continue
        dimensions = measures = time = 0
        for slot, role in dsd_components(view, cls):
            dsd_slots.add(slot.name)
            where = f"slot '{slot.name}' of the DSD '{cls.name}'"
            if role not in QB_COMPONENTS:
                problems.append(f"{where} has {QB_COMPONENT_ANNOTATION} '{role}'; it is dimension or measure")
                continue
            if not slot.required:
                problems.append(f"{where} is a {role} and must be required: an observation without it is no cell of the table")
            if slot.multivalued:
                problems.append(f"{where} is a {role} and holds one value per observation, so it cannot be multivalued")
            kind = ngsi_ld_kind(slot)
            if role == "dimension":
                dimensions += 1
                if kind == "Property" and (slot.range or "string") in QB_TIME_RANGES:
                    time += 1
                elif kind not in ("VocabProperty", "Relationship"):
                    problems.append(
                        f"{where} is a dimension of kind {kind} with range {slot.range or 'string'}; a dimension is a "
                        "VocabProperty, a Relationship, or the one time dimension (a Property of range "
                        + ", ".join(QB_TIME_RANGES) + ")"
                    )
            else:
                measures += 1
                if kind != "Property":
                    problems.append(f"{where} is a measure of kind {kind}; a measure is a Property")
                elif unit_of(slot) is None:
                    problems.append(f"{where} is a measure without a unit; give it one (DM-59)")
        if time > 1:
            problems.append(f"the DSD '{cls.name}' has {time} time dimensions; it has one at most, the rest are VocabProperty terms")
        if dimensions == 0 or measures == 0:
            problems.append(f"the DSD '{cls.name}' needs at least one dimension and one measure (qb_component on its slots)")
    for slot in view.all_slots().values():
        if component_of(slot) is not None and slot.name not in dsd_slots:
            problems.append(
                f"slot '{slot.name}' declares {QB_COMPONENT_ANNOTATION} but no class using it is annotated "
                f"{QB_DSD_ANNOTATION}: true"
            )
    if problems:
        raise ModelError("the Data Structure Definition is not a cube: " + "; ".join(problems))


def _view(path: str) -> SchemaView:
    """A view whose imports are already merged in.

    Every LinkML generator builds its own `SchemaView` from the schema it is handed, and that
    one has no import map, so an unmerged schema loses `ngsi-ld-core` the moment it reaches a
    generator. Merging once here is also what makes the artifacts self-contained: a consumer
    of the JSON Schema or the SHACL shapes has no way to resolve our imports.
    """
    view = SchemaView(path, importmap={**_REQUEST_IMPORTS.get(), **IMPORT_MAP})
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
        # The CEFACT common code and the QUDT unit IRI travel side by side in exact_mappings:
        # the first is what NGSI-LD puts on the wire as `unitCode`, the second is what a
        # federated reader dereferences to align two organisations' measurements (DM-06,
        # DM-59).
        "exactMappings": list(getattr(unit, "exact_mappings", None) or []),
        # The dimension, which is what makes two units comparable at all: degrees Celsius and
        # degrees Fahrenheit are the same quantity kind, and micrograms per cubic metre are not.
        "hasQuantityKind": _text(getattr(unit, "has_quantity_kind", None)),
    }
    present = {k: v for k, v in fields.items() if v}
    return present or None


def _text(value: Any) -> str | None:
    """A LinkML `uriorcurie` as plain text, which is what an artifact carries."""
    return str(value) if value else None


def slots_of(view: SchemaView, cls: ClassDefinition) -> list[SlotDefinition]:
    """Every slot of a class, induced so inherited and imported slots are included."""
    return [view.induced_slot(name, cls.name) for name in view.class_slots(cls.name)]
