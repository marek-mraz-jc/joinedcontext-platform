"""The HTTP face of Model Tools (T-0377, DM-10, DM-12, DM-17, DM-18, DM-19).

Two callers and one contract (API/01 §11). The Portal's LinkML editor posts a source on every
pause in typing and renders what comes back; the import wizard browses the Smart Data Models
catalogue and asks for one model of it. Both reach Model Tools through the Portal and never
directly, so this app has no ingress, no session, no credentials and no state that outlives a
request except the catalogue cache DM-12 asks for.

```text
GET  /healthz     is the process up, and which generator version is in this image
GET  /catalog     the Smart Data Models catalogue index, cached (`?refresh=true` refills now,
                  `?subject=` fills that subject's attribute names)
POST /generate    {"source": "<LinkML YAML>"}  -> the artifact set
POST /import-sdm  {"model": "dataModel.X/Y"}   -> the same set, plus the LinkML it produced
```

The routes call the same functions as the command line, so a preview, `jcctl model generate`
and CI cannot disagree about what a model compiles to (DM-19).
"""

from __future__ import annotations

import argparse
import ast
import base64
import binascii
import json
import os
import socketserver
import threading
import time
from datetime import datetime, timezone
from http import HTTPStatus
from typing import Any, Callable, Iterable
from urllib.parse import parse_qs
from wsgiref.simple_server import WSGIRequestHandler, WSGIServer, make_server

import yaml

from common import RelationshipError, as_path, generator_version
from gen_context import compile_context
from gen_docs import compile_docs
from gen_example import compile_example
from gen_json_schema import compile_schema
from gen_rdf_artifacts import compile_owl, compile_shacl
from gen_typescript import compile_typescript
from infer_schema import MAX_SAMPLE_BYTES, SampleError, infer
from import_sdm import (
    RAW_BASE,
    SCHEMA_FILE,
    ImportError_,
    _composed,
    _get,
    convert,
    fetch,
    resolve_commit,
    split_identifier,
)

#: Largest body this service reads, the same cap the Portal applies before forwarding (DM-18).
#: Model Tools is shared and stateless, so it refuses an oversized source itself rather than
#: trusting that the only caller already did.
MAX_BODY_BYTES = 512 * 1024
#: `/infer-schema` carries a sample of up to 10 MiB (DM-55), base64 in JSON: a third more.
MAX_INFER_BODY_BYTES = MAX_SAMPLE_BYTES * 4 // 3 + 4096

#: Largest catalogue document accepted from upstream. The index is a few megabytes and grows
#: slowly; a response far past that is a mirror gone wrong, not a catalogue.
MAX_CATALOGUE_BYTES = 16 * 1024 * 1024

#: The catalogue index, and the per-model titles and descriptions the wizard searches. Both
#: are inside the Smart Data Models organisation, so `_get`'s allowlist covers them (DM-10).
CATALOGUE_URL = f"{RAW_BASE}data-models/master/specs/AllSubjects/official_list_data_models.json"
CATALOGUE_DETAILS_URL = f"{RAW_BASE}data-models/master/specs/AllSubjects/metadata.json"

#: DM-12: a daily refresh, and an explicit refresh-now for the person who cannot wait for it.
CATALOGUE_TTL_SECONDS = 24 * 60 * 60

#: The artifact renderers, keyed by the field the Portal deserialises them into (API/01 §11).
#: Renaming a key here is a silent empty preview in the editor, which `test_service.py` guards.
#: The first four are the set DM-02 commits beside the source, in that order; SHACL and OWL
#: are DM-44's artifact-store set and are rendered in the same run so they cannot disagree.
#: `typescript` is the `jc-types.ts` a generated application compiles against (SDK-10).
RENDERERS: tuple[tuple[str, Callable[[str], Any]], ...] = (
    ("jsonSchema", compile_schema),
    ("context", compile_context),
    ("docs", compile_docs),
    ("example", compile_example),
    ("shacl", compile_shacl),
    ("owl", compile_owl),
    ("typescript", compile_typescript),
)


def artifacts(
    source: str, *, linkml: str | None = None, example: Any | None = None
) -> dict[str, Any]:
    """Render every artifact of one LinkML source, or say why the model does not compile.

    A half-written model is the normal state of an editor, so a source that does not parse is
    an answer with `errors` and no artifacts, never a server error (API/01 §11). Each
    generator runs on its own: a model whose OWL fails still previews its JSON Schema. A
    message every generator reports is the source itself being wrong and is said once.
    """
    rendered: dict[str, Any] = {"generatorVersion": generator_version()}
    if linkml is not None:
        rendered["linkml"] = linkml
    if example is not None:
        rendered["example"] = example

    failures: dict[str, list[str]] = {}
    broken: list[str] = []
    # Spooled once, so a parse error reads the same from all four and is reported once, and so
    # the temporary path never reaches the person editing the model.
    # ponytail: each generator still parses the file again, about a second for the four of
    # them. Share one SchemaView across them if the preview ever has to be faster.
    with as_path(source) as path:
        for field, render in RENDERERS:
            if field in rendered:
                # An import carries the catalogue's own example down with it, and real data
                # beats a value derived from a range, so the generated one does not replace it.
                continue
            try:
                rendered[field] = render(path)
            except RelationshipError as err:
                # Every generator loads the model and refuses it the same way: said once, one
                # entry per broken rule (DM-68).
                broken = err.problems
            except Exception as err:  # noqa: BLE001 - a generator crash is a message, not a 500
                failures.setdefault(str(err).replace(path, "<source>"), []).append(field)

    if broken:
        return {"generatorVersion": rendered["generatorVersion"], "errors": broken}
    rendered["errors"] = [
        message if len(fields) == len(RENDERERS) else f"{', '.join(fields)}: {message}"
        for message, fields in failures.items()
    ]
    return rendered


def generate(body: dict[str, Any]) -> tuple[int, dict[str, Any]]:
    """`POST /generate`: compile the source the editor is holding."""
    source = body.get("source")
    if not isinstance(source, str) or not source.strip():
        return 400, {"errors": ["'source' must be the LinkML document as a string"]}
    return 200, artifacts(source)


def import_sdm(body: dict[str, Any]) -> tuple[int, dict[str, Any]]:
    """`POST /import-sdm`: fetch one catalogue model and compile what it becomes.

    The identifier is checked before anything opens a socket (DM-10). Everything after that is
    the import failing, which the person running the wizard has to read, so it comes back as
    `errors` on a 200 rather than as a status the Portal can only report as unreachable.
    """
    model = body.get("model")
    if not isinstance(model, str):
        return 400, {"errors": ["'model' must be a Smart Data Models catalogue identifier"]}
    try:
        split_identifier(model)
    except ImportError_ as err:
        return 400, {"errors": [str(err)]}

    try:
        fetched = fetch(model)
        document = convert(
            model,
            fetched["schema"],
            fetched["context"],
            fetched["example"],
            fetched["provenance"],
            fetched.get("commons"),
        )
    except Exception as err:  # noqa: BLE001 - upstream being down is the wizard's message too
        return 200, {"generatorVersion": generator_version(), "errors": [str(err)]}

    source = yaml.safe_dump(document, sort_keys=False, allow_unicode=True)
    return 200, artifacts(source, linkml=source, example=fetched["example"])


def infer_schema(body: dict[str, Any]) -> tuple[int, dict[str, Any]]:
    """`POST /infer-schema`: a draft model from one sample (T-0598, DM-54, DM-55).

    The body is `{"name": "<file name>", "content": "<base64>", "format"?: csv|xlsx|json|pdf}`.
    The sample is parsed in memory and nothing leaves the process: the catalogue alignment reads
    the attribute names this replica already holds and fetches none.
    """
    name = body.get("name")
    content = body.get("content")
    fmt = body.get("format")
    if not isinstance(name, str) or not isinstance(content, str):
        return 400, {"errors": ["'name' and 'content' (base64) are required"]}
    if fmt is not None and not isinstance(fmt, str):
        return 400, {"errors": ["'format' must be csv, xlsx, json or pdf"]}
    try:
        raw = base64.b64decode(content, validate=True)
    except (ValueError, binascii.Error):
        return 400, {"errors": ["'content' is not base64"]}
    try:
        answer = infer(name, raw, fmt, CATALOGUE.attribute_index())
    except SampleError as err:
        return 400, {"errors": [str(err)]}
    answer["generatorVersion"] = generator_version()
    return 200, answer


class Catalogue:
    """The Smart Data Models index, cached with a daily refresh (DM-12).

    The cache is per replica and in memory: Model Tools stays stateless in the sense DM-18
    means, nothing it holds has to survive a restart or be shared with the other replica.
    Unavailability degrades to the cached index with `stale: true` and never blocks editing;
    with nothing cached yet it degrades to an empty index, because an editor waiting on GitHub
    is the one outcome the requirement rules out.
    """

    def __init__(self) -> None:
        # One refresh at a time: the lock is held across the fetch on purpose, so ten editors
        # opening the wizard at once produce one request upstream and not ten. It is held
        # across an attribute fill for the same reason, which is also why a fill is per subject
        # and not per model.
        self._lock = threading.Lock()
        self._subjects: list[dict[str, Any]] | None = None
        self._attributes: dict[str, dict[str, list[str]]] = {}
        self._filled_at: float = 0.0
        self._refreshed_at: str | None = None
        self._stale = False

    def index(self, refresh: bool = False, subject: str | None = None) -> dict[str, Any]:
        with self._lock:
            fresh = (
                self._subjects is not None
                and time.time() - self._filled_at < CATALOGUE_TTL_SECONDS
            )
            if refresh or not fresh:
                self._fill()
            if subject:
                self._fill_attributes(subject)
            return {
                "subjects": self._subjects or [],
                "refreshedAt": self._refreshed_at,
                "stale": self._stale or self._subjects is None,
            }

    def attribute_index(self) -> dict[str, list[str]]:
        """Every attribute name this replica has seen, to the catalogue models declaring it.
        Read-only: inference must not turn into a fetch (DM-55)."""
        with self._lock:
            index: dict[str, list[str]] = {}
            for subject, models in self._attributes.items():
                for model, names in models.items():
                    for name in names:
                        index.setdefault(name, []).append(f"{subject}/{model}")
            return index

    def _fill(self) -> None:
        try:
            self._subjects = fetch_catalogue()
        except Exception:  # noqa: BLE001 - DM-12: the cached index answers, the editor works
            self._stale = True
            return
        self._filled_at = time.time()
        self._refreshed_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        self._stale = False
        # A daily refresh must not cost the wizard the subjects it already opened.
        for name in list(self._attributes):
            self._attach(name)

    def _fill_attributes(self, subject: str) -> None:
        """Fill one subject's attribute names, once (T-0404, DM-12).

        Only a subject the index already lists is fetched. That is not a performance guard:
        `?subject=` is a value a browser sends, and a catalogue name is the only thing it may
        turn into a request upstream.
        """
        entry = next((s for s in self._subjects or [] if s["name"] == subject), None)
        if entry is None:
            return
        if subject not in self._attributes:
            try:
                self._attributes[subject] = fetch_attributes(
                    subject, [model["name"] for model in entry["models"]]
                )
            except Exception:  # noqa: BLE001 - the subject still lists its models
                return
        self._attach(subject)

    def _attach(self, subject: str) -> None:
        """Put the cached attribute names back on the models of one subject."""
        filled = self._attributes.get(subject) or {}
        for entry in self._subjects or []:
            if entry["name"] != subject:
                continue
            for model in entry["models"]:
                names = filled.get(model["name"])
                if names is not None:
                    model["attributes"] = names


def _document(url: str) -> Any:
    """One catalogue document, size-capped and parsed.

    `metadata.json` is published as a Python literal rather than JSON, so it is read with
    `ast.literal_eval`, which evaluates literals and nothing else — never `eval`, on a
    document fetched over the network.
    """
    response = _get(url)
    if len(response.content) > MAX_CATALOGUE_BYTES:
        raise ImportError_(f"{url} answered {len(response.content)} bytes, past the cap")
    text = response.content.decode("utf-8")
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return ast.literal_eval(text)


def fetch_catalogue() -> list[dict[str, Any]]:
    """The catalogue index as the wizard browses it: subjects, and the models under them.

    The index carries no URLs (API/01 §11). A model is named by its catalogue identifier and
    fetching one is `import-sdm`, so nothing a browser sees can point the fetch anywhere.
    """
    official = _document(CATALOGUE_URL)
    described = {
        (entry.get("subject"), entry.get("dataModel")): entry
        for entry in _details()
        if isinstance(entry, dict)
    }

    subjects: list[dict[str, Any]] = []
    for entry in official.get("officialList", []):
        repository = entry.get("repoName")
        if not isinstance(repository, str) or not repository:
            continue
        models = []
        for name in entry.get("dataModels") or []:
            detail = described.get((repository, name), {})
            model: dict[str, Any] = {"id": f"{repository}/{name}", "name": name}
            description = detail.get("description")
            if description:
                model["description"] = description
            # No attributes here: the catalogue publishes no aggregate carrying them, so
            # they are filled per subject when the wizard opens one (`fetch_attributes`).
            models.append(model)
        subjects.append(
            {
                "name": repository,
                "title": repository.removeprefix("dataModel."),
                "models": models,
            }
        )
    return subjects


def fetch_attributes(subject: str, models: list[str]) -> dict[str, list[str]]:
    """The attribute names of every model of one subject, keyed by model name (DM-12).

    The catalogue publishes no document carrying the attributes of all 1118 models, so there
    is nothing to add to the index and nothing to refresh daily: the names exist only in each
    model's own `schema.json`. Fetching all of them is neither a startup cost nor a daily one
    anybody wants, and the wizard browses per subject anyway, so a subject is the unit — about
    fifteen requests for `dataModel.Environment`, once, cached until the index is refetched.

    Only the model's own attributes are read. A catalogue schema is an `allOf` of the shared
    commons by `$ref` and one inline branch; the commons carry `id`, `type`, `name`, `owner`
    and the rest, which every model has and no search distinguishes anything by, so the `$ref`
    is deliberately left unresolved and costs no second request.

    A model whose schema does not answer is left without attributes rather than failing the
    subject: an unsearchable model is worse than a subject the wizard cannot open at all.
    """
    commit = resolve_commit(subject)
    filled: dict[str, list[str]] = {}
    for name in models:
        try:
            schema = _get(f"{RAW_BASE}{subject}/{commit}/{name}/{SCHEMA_FILE}").json()
            properties = _composed(schema, "properties")
        except Exception:  # noqa: BLE001 - one model missing is not the subject failing
            continue
        if isinstance(properties, dict):
            filled[name] = sorted(properties)
    return filled


def _details() -> list[Any]:
    """Titles and descriptions per model. Missing them costs search, not the index."""
    try:
        details = _document(CATALOGUE_DETAILS_URL)
    except Exception:  # noqa: BLE001
        return []
    return details if isinstance(details, list) else []


CATALOGUE = Catalogue()


def catalog(query: dict[str, str]) -> tuple[int, dict[str, Any]]:
    """`GET /catalog`: the index, refilled first when the caller asks for a refresh.

    `?subject=` is the wizard opening one subject, and the models of that subject come back
    carrying their attribute names. Without it the answer costs one fetch, which is what keeps
    the first paint of a catalogue of 1118 models cheap (T-0404).
    """
    return 200, CATALOGUE.index(
        refresh=query.get("refresh", "").lower() == "true",
        subject=query.get("subject") or None,
    )


def healthz() -> tuple[int, dict[str, Any]]:
    """What a readiness probe asks, and which generator this image carries (DM-19)."""
    return 200, {"status": "ok", "generatorVersion": generator_version()}


def _read_body(environ: dict[str, Any], cap: int = MAX_BODY_BYTES) -> dict[str, Any]:
    """The request body as JSON, refusing one past the cap before it is read (DM-18)."""
    declared = environ.get("CONTENT_LENGTH") or "0"
    length = int(declared) if declared.isdigit() else 0
    if length > cap:
        raise ValueError("body larger than the payload limit")
    raw = environ["wsgi.input"].read(min(length, cap))
    body = json.loads(raw or b"{}")
    if not isinstance(body, dict):
        raise ValueError("the body must be a JSON object")
    return body


def _query(environ: dict[str, Any]) -> dict[str, str]:
    parsed = parse_qs(environ.get("QUERY_STRING", ""), keep_blank_values=True)
    return {key: values[-1] for key, values in parsed.items()}


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    """The WSGI app. Every answer is JSON, including the ones nothing routed to."""
    method = environ.get("REQUEST_METHOD", "GET")
    path = "/" + environ.get("PATH_INFO", "").strip("/")

    try:
        if method == "GET" and path == "/healthz":
            status, payload = healthz()
        elif method == "GET" and path == "/catalog":
            status, payload = catalog(_query(environ))
        elif method == "POST" and path == "/generate":
            status, payload = generate(_read_body(environ))
        elif method == "POST" and path == "/import-sdm":
            status, payload = import_sdm(_read_body(environ))
        elif method == "POST" and path == "/infer-schema":
            status, payload = infer_schema(_read_body(environ, MAX_INFER_BODY_BYTES))
        else:
            status, payload = 404, {"errors": [f"{method} {path} is not a Model Tools route"]}
    except ValueError as err:
        # An unreadable or oversized body. The message is about the request, never about what
        # is inside it: the body may be someone's unpublished model.
        oversized = "larger than" in str(err)
        status, payload = (413 if oversized else 400), {"errors": [str(err)]}

    body = json.dumps(payload).encode("utf-8")
    start_response(
        f"{status} {HTTPStatus(status).phrase}",
        [("Content-Type", "application/json"), ("Content-Length", str(len(body)))],
    )
    return [body]


class _Handler(WSGIRequestHandler):
    # A client that opens a connection and stops talking must not hold a worker forever.
    timeout = 30


class _Server(socketserver.ThreadingMixIn, WSGIServer):
    # ponytail: the stdlib server, because generation is CPU-bound and holds the GIL, so an
    # async framework would buy nothing and cost a dependency. Put a real WSGI server in front
    # if Model Tools ever faces anything but the Portal.
    daemon_threads = True


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--host", default=os.environ.get("MODEL_TOOLS_HOST", "0.0.0.0"))
    parser.add_argument(
        "--port", type=int, default=int(os.environ.get("MODEL_TOOLS_PORT", "8080"))
    )
    args = parser.parse_args(argv)
    with make_server(
        args.host, args.port, application, server_class=_Server, handler_class=_Handler
    ) as server:
        print(f"model-tools {generator_version()} on {args.host}:{args.port}", flush=True)
        server.serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
