#!/usr/bin/env python3
"""The hub RAG probe (T-5368): asks search_documents of a hub each question of
crates/assistant/tests/fixtures/hub_rag_probes.json and prints one line per question, then the
totals the task's acceptance names: the right publisher first, a relevant page in the top 3, no
chrome-only passage and no repeated URL in any top 5.

    scripts/hub-rag-probe.py [https://api.dev.joinedcontext.com/api/mcp] > evidence/T-5368/after.txt

Anonymous: the hub's search_documents needs no token. Paced at one call a second, under the
tool's 30 a minute per caller.
"""

import json
import re
import sys
import time
import unicodedata
import urllib.request
from pathlib import Path

HUB = sys.argv[1] if len(sys.argv) > 1 else "https://api.dev.joinedcontext.com/api/mcp"
PROBES = Path(__file__).resolve().parent.parent / "crates/assistant/tests/fixtures/hub_rag_probes.json"
# The same rule as jc-assistant's extract::chrome_only: under 40 characters of prose once images,
# links (text and all) and headings are out, a passage is icons and a menu alone.
MIN_OWN_PROSE = 40


def fold(text):
    text = unicodedata.normalize("NFKD", text or "")
    return "".join(c for c in text if not unicodedata.combining(c)).lower()


def prose(text):
    text = re.sub(r"!\[[^\]]*\]\([^)]*\)", " ", text or "")
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", " ", text)
    lines = [l for l in text.splitlines() if not l.lstrip().startswith("#")]
    return " ".join(" ".join(lines).replace("*", " ").replace("|", " ").split())


def site(url):
    return re.sub(r"^https?://(www\.)?", "", url or "")


def ask(query):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                       "params": {"name": "search_documents", "arguments": {"query": query, "limit": 5}}})
    request = urllib.request.Request(HUB, data=body.encode(), headers={
        "content-type": "application/json", "accept": "application/json, text/event-stream"})
    with urllib.request.urlopen(request, timeout=30) as answer:
        result = json.load(answer)["result"]
    return (result.get("structuredContent") or {}).get("hits", [])


def main():
    probes = json.loads(PROBES.read_text())
    totals = {"publisher": 0, "relevant": 0, "chrome": 0, "repeated": 0}
    print("ok-pub | top3 | chrome | dup | query | first publisher | first url")
    for probe in probes:
        hits = ask(probe["query"])
        time.sleep(1)
        first = hits[0] if hits else {}
        right = first.get("publisher") == probe["publisher"]
        relevant = any(
            site(h.get("url")).startswith(site(probe["url"]))
            and any(w in fold(h.get("url", "") + " " + (h.get("title") or "") + " " + h.get("text", "")[:400])
                    for w in probe["relevant"])
            for h in hits[:3])
        chrome = sum(len(prose(h.get("text"))) < MIN_OWN_PROSE for h in hits)
        urls = [h.get("url", "").split("#")[0] for h in hits]
        repeated = len(urls) - len(set(urls))
        totals["publisher"] += right
        totals["relevant"] += relevant
        totals["chrome"] += chrome > 0
        totals["repeated"] += repeated > 0
        print(f"{'yes' if right else 'NO'} | {'yes' if relevant else 'NO'} | {chrome} | {repeated} | "
              f"{probe['query']} | {first.get('publisher')} | {first.get('url')}")
    n = len(probes)
    print(f"\nright publisher first {totals['publisher']}/{n} (need 27), relevant in top 3 "
          f"{totals['relevant']}/{n} (need 24), with a chrome-only passage {totals['chrome']} (need 0), "
          f"with a repeated URL {totals['repeated']} (need 0)")


if __name__ == "__main__":
    main()
