#!/usr/bin/env python3
"""One request from a Portal page through the context-gateway to the Antares broker store (T-3917).

cg keeps one repository per graph, so this script asks three graphs and joins them on the HTTP
channel keys: the Portal's call sites (a GREP of `ui/src`, `sdk/src` and `src`, the calls being
built at run time), the gateway's routes and its pinned broker hops (`.cg/links/gateway.toml`),
and Antares' NGSI-LD routes (its own `.cg/links/axum.toml`). For each of five operations it
prints the chain with the identity, tenant and decision hops marked, and fails when a chain
reaches the store without a policy decision before it, without a pinned tenant, or when any hop
on it reads a space or a right out of an entity URN (ADR-N-041).

Each repository needs a graph built from the tree to trace (`cg build --fg` in each).

  dev/trace-request.py [--portal DIR] [--broker DIR]   write docs/analysis/request-trace.md
  dev/trace-request.py --check                          check only
"""

from __future__ import annotations

import argparse
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
PAGE = ROOT / "docs" / "analysis" / "request-trace.md"

# The five operations: the Portal call sites, the NGSI-LD key under the root, whether the broker's
# view serves it (`served_by_view` in app.rs: subscriptions and If-Match writes stay on the
# gateway's own decision), and the store method the broker's handler ends in.
OPERATIONS = [
    ("create entity", r"ngsi-ld/v1/entities`;|method: \"POST\", path: `[^`]*ngsi-ld/v1/entities`",
     "POST /ngsi-ld/v1/entities", True, "antares_store::CurrentStateDriver::create"),
    ("PATCH attribute", r"ngsi-ld/v1/entities/\$\{[^}]+\}/attrs|\"PATCH\", new RegExp\(`\^ngsi-ld",
     "PATCH /ngsi-ld/v1/entities/{}/attrs", True, "antares_store::<S as CurrentStateDriverExt>::mutate"),
    ("query with q=", r"ngsi-ld/v1/entities\?",
     "GET /ngsi-ld/v1/entities", True, "antares_store::CurrentStateDriver::query_entities"),
    ("subscribe", r"ngsi-ld/v1/subscriptions\"",
     "POST /ngsi-ld/v1/subscriptions", False, "antares_store::CurrentStateDriver::create"),
    ("temporal read", r"ngsi-ld/v1/temporal/entities[?/]",
     "GET /ngsi-ld/v1/temporal/entities", True, "antares_store::TemporalDriver::query_temporal"),
]
PORTAL_FILES = "ui/src/**/*.ts, ui/src/**/*.tsx, sdk/src/**/*.ts, src/**/*.rs"

# The hops the trace marks, by role.
GATEWAY_MARKS = {
    "context_gateway::middleware::tenancy::strip_client_headers": "identity: client x-jc-*/tenant headers dropped",
    "context_gateway::app::admit": "identity: token verified, audience = the Endpoint",
    "context_gateway::app::admit_space": "identity: token verified, audience = the space",
    "context_gateway::middleware::tenancy::pin_caller": "tenant + identity: NGSILD-Tenant = Endpoint's space, token/cookie dropped, x-jc-* = verified subject",
    "context_gateway::pdp::Pdp::decide": "decision: the gateway's PDP",
}
BROKER_MARKS = {
    "antares_ops::ViewOps::run": "tenant: re-pinned to the view's one tenant, another refused",
    "antares_api::policy::through_view": "decision scope: the view, from the in-process scope",
    "antares_api::negotiate::tenant_from": "tenant: NGSILD-Tenant read",
    "antares_api::policy::gate": "decision: the policy engine",
}
VIEW_HOPS = ["antares_views::serve", "antares_ops::ViewOps::run", "antares_ops::Ops::run_in", "antares_api::state::AppState::call"]
URN_READERS = ["space", "org_domain", "prefixed"]  # jc_core::urn::Urn methods that name a space


def cg(repo: pathlib.Path, statements: str) -> list[list[list[str]]]:
    run = subprocess.run(["cg", "q", "--max-rows", "2000", "-"], input=statements, cwd=repo,
                         capture_output=True, text=True, check=False)
    if run.returncode != 0:
        sys.exit(f"cg q in {repo}: {run.stdout}{run.stderr}")
    found: list[list[list[str]]] = []
    rows: list[list[str]] | None = None
    header = False
    for line in run.stdout.splitlines():
        if line.startswith("### Q"):
            rows, header = [], True
            found.append(rows)
        elif rows is None or not line.strip() or re.match(r"\d+ (rows|hits)", line):
            header = header or bool(re.match(r"\d+ hits", line))
        elif "output stops at" in line or "truncated" in line or line.startswith("ERROR"):
            sys.exit(f"cg q in {repo}: {line}")
        elif line.startswith("hint:"):
            continue
        elif header:
            header = False
        else:
            rows.append([cell.strip() for cell in line.split(" | ")])
    return found


def q(text: str) -> str:
    return "'" + text.replace("\\", "\\\\").replace("'", "\\'") + "'"


def path_of(rows: list[list[str]]) -> list[str]:
    """The node paths of a SHORTEST path, one row per node (a list cell would be cut short)."""
    return [row[0] for row in rows]


def shortest(a: str, b: str) -> str:
    return (f"MATCH p = (a:Symbol {{path:{q(a)}}})-[:CALLS|DISPATCHES* SHORTEST 1..8]->"
            f"(b:Symbol {{path:{q(b)}}}) UNWIND nodes(p) AS n RETURN n.path;")


def first_call_line(repo: pathlib.Path, parent: str, child: str) -> int:
    rows = cg(repo, f"MATCH (a:Symbol {{path:{q(parent)}}})-[c:CALLS|DISPATCHES]->(b:Symbol {{path:{q(child)}}}) "
                    "RETURN min(c.line);")[0]
    return int(rows[0][0]) if rows and rows[0][0] not in ("", "None") else 0


def before(repo: pathlib.Path, decision: list[str], store: list[str]) -> bool:
    """Whether the decision path leaves the two paths' last shared caller before the store path."""
    shared = 0
    while shared < min(len(decision), len(store)) - 1 and decision[shared + 1] == store[shared + 1]:
        shared += 1
    parent = decision[shared]
    return 0 < first_call_line(repo, parent, decision[shared + 1]) <= first_call_line(repo, parent, store[shared + 1])


def trace(portal: pathlib.Path, broker: pathlib.Path) -> tuple[list[str], list[str]]:
    lines, problems = [], []
    gw_routes, gw_uses = cg(ROOT, """\
MATCH (h:Symbol)-[p:PROVIDES]->(c:Channel {channel:'http'}) WHERE c.key STARTS WITH 'ANY ' AND c.key ENDS WITH '/ngsi-ld/v1/{}' RETURN DISTINCT c.key, h.path;
MATCH (h:Symbol)-[:USES]->(c:Channel {channel:'http'}) RETURN c.key, h.path;
""")
    for name, portal_grep, root_key, by_view, store_fn in OPERATIONS:
        verb, under = root_key.split(" ", 1)
        lines += [f"## {name}", "", "```"]
        callers = cg(portal, f"GREP {portal_grep} IN {PORTAL_FILES};")[0]
        pages = sorted({row[1] for row in callers
                        if len(row) > 3 and row[2] == "False" and row[3] == "False" and row[1] and "testing" not in row[0]})
        if not pages:
            problems.append(f"{name}: no Portal call site matches {portal_grep!r}")
        lines.append("Portal      " + ("\n            ".join(pages) or "(none)"))
        lines.append("edge        APISIX (joinedcontext-deployment; not in a code graph)")

        # Gateway: the catch-all route, its handler, the sender that USES the broker key.
        senders = [("context_gateway::app::serve_ngsi_ld_view", f"{verb} /x/views/{{}}{under}")] if by_view else []
        senders.append(("context_gateway::app::serve_ngsi_ld", root_key))
        for key, handler in gw_routes:
            marks = cg(ROOT, "\n".join(
                f"MATCH (h:Symbol {{path:{q(handler)}}})-[:CALLS|DISPATCHES*1..6]->(g:Symbol {{path:{q(m)}}}) RETURN count(*);"
                for m in GATEWAY_MARKS))
            reached = [m for m, rows in zip(GATEWAY_MARKS, marks) if rows and rows[0][0] != "0"]
            lines.append(f"gateway     {key} -> {handler}")
            for m in reached:
                lines.append(f"              * {m.rsplit('::', 1)[-1]}: {GATEWAY_MARKS[m]}")
            for needed in ("context_gateway::middleware::tenancy::strip_client_headers",
                           "context_gateway::middleware::tenancy::pin_caller"):
                if needed not in reached:
                    problems.append(f"{name}: {handler} does not reach {needed}")
            if not {"context_gateway::app::admit", "context_gateway::app::admit_space"} & set(reached):
                problems.append(f"{name}: {handler} admits no caller")
        for sender, key in senders:
            if [key, sender] not in gw_uses:
                problems.append(f"{name}: {sender} has no pinned use of {key} (.cg/links/gateway.toml)")
            decided = sender.endswith("serve_ngsi_ld") and cg(ROOT, shortest(sender, "context_gateway::pdp::Pdp::decide"))[0]
            lines.append(f"  -> {sender} -> broker {key}" + ("   [gateway PDP decides first]" if decided else ""))
            if sender.endswith("serve_ngsi_ld") and not decided:
                problems.append(f"{name}: {sender} reaches no Pdp::decide")

        # Broker: the view hops, then the root route's handler down to the store.
        handlers = cg(broker, f"MATCH (h:Symbol)-[:PROVIDES]->(c:Channel {{key:{q(root_key)}}}) RETURN h.path;")[0]
        if not handlers:
            problems.append(f"{name}: Antares provides no {root_key}")
            lines += ["```", ""]
            continue
        handler = handlers[0][0]
        if by_view:
            hops = [path_of(cg(broker, shortest(a, b))[0]) for a, b in zip(VIEW_HOPS, VIEW_HOPS[1:])]
            if not all(hops):
                problems.append(f"{name}: the view chain {' -> '.join(VIEW_HOPS)} is broken in the graph")
            lines.append(f"broker      ANY /x/views/{{}}/ngsi-ld/v1/{{}} -> {' -> '.join(VIEW_HOPS)}")
            for m in VIEW_HOPS:
                if m in BROKER_MARKS:
                    lines.append(f"              * {m.rsplit('::', 1)[-1]}: {BROKER_MARKS[m]}")
            lines.append(f"              * run_in -> policy::through_view: {BROKER_MARKS['antares_api::policy::through_view']}")
            lines.append(f"            in-process {root_key} -> {handler}")
        else:
            lines.append(f"broker      {root_key} -> {handler}")
        gate = path_of(cg(broker, shortest(handler, "antares_api::policy::gate"))[0])
        tenant = path_of(cg(broker, shortest(handler, "antares_api::negotiate::tenant_from"))[0])
        store = path_of(cg(broker, shortest(handler, store_fn))[0])
        for label, path in (("tenant", tenant), ("decision", gate), ("store", store)):
            lines.append(f"              {label:9} {' -> '.join(p.rsplit('::', 2)[-2] + '::' + p.rsplit('::', 1)[-1] for p in path) or '(none)'}")
        if not gate:
            problems.append(f"{name}: {handler} reaches no policy::gate")
        if not tenant:
            problems.append(f"{name}: {handler} reads no NGSILD-Tenant")
        if not store:
            problems.append(f"{name}: {handler} does not reach {store_fn}")
        else:
            for label, path in (("policy::gate", gate), ("negotiate::tenant_from", tenant)):
                if path and not before(broker, path, store):
                    problems.append(f"{name}: {handler} calls the store ({store[-1]}) before {label}")
        lines += ["```", ""]
    return lines, problems


def urn_readers(broker: pathlib.Path) -> tuple[list[str], list[str]]:
    """ADR-N-041: who on the request path reads a space or a domain out of an entity URN."""
    gw = cg(ROOT, f"""\
MATCH (h:Symbol)-[:PROVIDES]->(c:Channel {{channel:'http'}}) WHERE c.key CONTAINS '/ngsi-ld/v1/' MATCH (h)-[:CALLS|DISPATCHES*1..8]->(x:Symbol)-[:CALLS]->(m:Symbol) WHERE m.path STARTS WITH 'jc_core::urn::Urn::' AND m.name IN [{', '.join(q(n) for n in URN_READERS)}] RETURN DISTINCT x.path, m.name;
""")[0]
    ant = cg(broker, """\
MATCH (h:Symbol)-[:CALLS|DISPATCHES*1..6]->(g:Symbol) WHERE h.path IN ['antares_views::serve', 'antares_ops::ViewOps::run', 'antares_api::policy::gate', 'antares_api::negotiate::tenant_from'] AND g.name =~ '(?i).*(urn|nss|org_domain|space_of).*' AND NOT g.external AND NOT g.is_test RETURN DISTINCT h.path, g.path;
""")[0]
    found = [f"gateway: {x} calls Urn::{m}" for x, m in gw] + [f"broker: {h} reaches {g}" for h, g in ant]
    return found, [f"ADR-N-041: {f}" for f in found]


def selftest() -> int:
    """`before` on made-up call lines: the decision's branch must leave first, and a branch the
    graph has no line for never counts as first."""
    global cg
    lines = {("h", "d"): 10, ("h", "s"): 20, ("x", "s2"): 5, ("x", "d2"): 9}

    def fake(_repo: pathlib.Path, statement: str) -> list[list[list[str]]]:
        a, b = re.findall(r"path:'([^']*)'", statement)
        return [[[str(lines.get((a, b), ""))]]]

    real, cg = cg, fake
    try:
        assert before(ROOT, ["h", "d"], ["h", "s"])
        assert not before(ROOT, ["x", "d2"], ["x", "s2"]), "the store's branch leaves first"
        assert not before(ROOT, ["h", "nowhere"], ["h", "s"]), "no line is not first"
        assert before(ROOT, ["r", "h", "d"], ["r", "h", "s"]), "the last shared caller decides"
    finally:
        cg = real
    print("trace-request selftest: ok")
    return 0


def main(argv: list[str]) -> int:
    if argv[1:] == ["--selftest"]:
        return selftest()
    args = argparse.ArgumentParser()
    args.add_argument("--portal", type=pathlib.Path, default=ROOT.parent / "joinedcontext-portal")
    args.add_argument("--broker", type=pathlib.Path, default=ROOT.parent / "Antares-NGSI-LD-Context-Broker")
    args.add_argument("--check", action="store_true")
    opts = args.parse_args(argv[1:])
    shas = {name: subprocess.run(["cg", "q", "-e", "MATCH (m:Meta) RETURN m.key, m.value;"], cwd=repo,
                                 capture_output=True, text=True).stdout.splitlines()[0]
            for name, repo in (("portal", opts.portal), ("platform", ROOT), ("broker", opts.broker))}
    chains, problems = trace(opts.portal, opts.broker)
    readers, urn_problems = urn_readers(opts.broker)
    problems += urn_problems
    for p in problems:
        print(p)
    if not opts.check:
        PAGE.parent.mkdir(parents=True, exist_ok=True)
        PAGE.write_text("\n".join([
            "# Request trace: Portal to gateway to broker store",
            "",
            "Generated by `dev/trace-request.py` (T-3917) from three code graphs joined on their HTTP",
            "channel keys. Do not edit by hand: change the code or `.cg/links/gateway.toml` and run the",
            "script. A chain fails the script when it reaches the store without `policy::gate` called",
            "before the store, without the tenant read from the header the gateway and the view pin, or",
            "when a hop reads a space or a right out of an entity URN (ADR-N-041).",
            "",
            "Graphs:",
            "",
            *[f"- {name}: `{header}`" for name, header in shas.items()],
            "",
            "Each operation has two ways through the gateway. A request the broker's view serves",
            "(`serve_ngsi_ld_view`) is decided by the broker's policy engine under the `x-jc-*` subject",
            "the gateway verified. A request no view serves yet (a subscription, an `If-Match` write, a",
            "view of another model, a caller under an agreement) is decided by the gateway's PDP first",
            "(`serve_ngsi_ld`) and again by the engine at the broker's root.",
            "",
            *chains,
            "## URN segments (ADR-N-041)",
            "",
            *([f"- {r}" for r in readers] or ["No hop on these paths reads a space, a domain or a right out of an entity URN."]),
            "",
        ]))
    print(f"trace-request: {len(OPERATIONS)} operations, {len(problems)} problem(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
