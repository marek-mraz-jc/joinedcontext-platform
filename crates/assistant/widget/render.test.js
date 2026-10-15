// T-3325: the widget's Markdown subset and its sources. Run with `node --test crates/assistant/widget`.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const { markdown, sources, label, safeUrl, events, language, table, linkHost } = require("./render.js");

const upTo = (max) => (n) => n >= 1 && n <= max;

test("lines of a paragraph keep their breaks and a blank line starts the next", () => {
  assert.deepEqual(markdown("one\ntwo\n\nthree"), [
    { tag: "p", children: ["one", { tag: "br" }, "two"] },
    { tag: "p", children: ["three"] },
  ]);
});

test("bullet and numbered lists become lists, an indented line stays with its item", () => {
  assert.deepEqual(markdown("Events:\n- a\n* b\n  more\n1. c\n2) d"), [
    { tag: "p", children: ["Events:"] },
    { tag: "ul", children: [
      { tag: "li", children: ["a"] },
      { tag: "li", children: ["b", { tag: "br" }, "more"] },
    ] },
    { tag: "ol", children: [{ tag: "li", children: ["c"] }, { tag: "li", children: ["d"] }] },
  ]);
});

test("bold, italic and inline code, nested in a list item", () => {
  assert.deepEqual(markdown("- **Workshop for _Families_** at `10:00` and *noon*"), [
    { tag: "ul", children: [{ tag: "li", children: [
      { tag: "strong", children: ["Workshop for ", { tag: "em", children: ["Families"] }] },
      " at ",
      { tag: "code", children: ["10:00"] },
      " and ",
      { tag: "em", children: ["noon"] },
    ] }] },
  ]);
});

test("a heading reads as a bold paragraph", () => {
  assert.deepEqual(markdown("## Helsinki events"), [
    { tag: "p", children: [{ tag: "strong", children: ["Helsinki events"] }] },
  ]);
});

test("a snake_case name and a lone star stay text", () => {
  assert.deepEqual(markdown("query_entities_tool costs 2 * 3"), [
    { tag: "p", children: ["query_entities_tool costs 2 * 3"] },
  ]);
});

const anyHost = () => true;

test("an http(s) link is a link; any other scheme stays text", () => {
  assert.deepEqual(markdown("[the city](https://hel.fi/x) [bad](javascript:alert(1)) [ftp](ftp://x)", undefined, anyHost), [
    { tag: "p", children: [
      { tag: "a", href: "https://hel.fi/x", children: ["the city"] },
      " [bad](javascript:alert(1)) [ftp](ftp://x)",
    ] },
  ]);
});

test("HTML and a script tag in the answer stay text", () => {
  const blocks = markdown('<script>alert("x")</script> <img src=x onerror=alert(1)> **<b>bold</b>**');
  assert.deepEqual(blocks, [
    { tag: "p", children: [
      '<script>alert("x")</script> <img src=x onerror=alert(1)> ',
      { tag: "strong", children: ["<b>bold</b>"] },
    ] },
  ]);
});

test("citation markers become citations, an unknown number is dropped", () => {
  assert.deepEqual(markdown("**Helsinki events** dataset [2, 3] and [9] and [1, 9]", upTo(3)), [
    { tag: "p", children: [
      { tag: "strong", children: ["Helsinki events"] },
      " dataset ",
      { tag: "cite", numbers: [2, 3] },
      " and ",
      " and ",
      { tag: "cite", numbers: [1] },
    ] },
  ]);
});

test("the owner's sources: one per address, titled, and live data never by its tool", () => {
  const { list, position } = sources([
    { n: 1, url: "https://data.dev.joinedcontext.com/dataset/helsinki-events", title: "Helsinki events" },
    { n: 2, url: "https://data.dev.joinedcontext.com/dataset/helsinki-events" },
    { n: 3, tool: "query_entities", endpoint: "helsinki-events", url: "https://data.dev.joinedcontext.com/dataset/helsinki-events", title: "Helsinki events" },
    { n: 4, tool: "query_entities", endpoint: "helsinki-events" },
    { n: 5, url: "javascript:alert(1)" },
  ]);
  assert.deepEqual(list, [
    { url: "https://data.dev.joinedcontext.com/dataset/helsinki-events", title: "Helsinki events", live: false,
      endpoint: null, domain: "data.dev.joinedcontext.com", numbers: [1, 2, 3] },
    { url: null, title: null, live: true, endpoint: "helsinki-events", domain: "", numbers: [4] },
  ]);
  assert.deepEqual(position, { 1: 1, 2: 1, 3: 1, 4: 2 });
  assert.equal(label(list[0]), "Helsinki events");
  assert.equal(label(list[1]), "helsinki-events");
  assert.equal(label({ url: "https://www.hel.fi/en/events/", title: null }), "www.hel.fi/en/events");
  // T-4184: an uploaded file is its file name, never a link.
  assert.equal(label({ url: "upload:subory/Návod.md#page=1", title: null }), "Návod.md");
  assert.doesNotMatch(JSON.stringify(list), /query_entities/);
});

test("nothing to show for no answer and no citations", () => {
  assert.deepEqual(markdown(""), []);
  assert.deepEqual(sources(undefined), { list: [], position: {} });
});

test("neither script ever parses HTML: no innerHTML and no way around it", () => {
  const fs = require("node:fs");
  const path = require("node:path");
  for (const file of ["render.js", "widget.js"]) {
    const code = fs.readFileSync(path.join(__dirname, file), "utf8");
    assert.doesNotMatch(code, /innerHTML|outerHTML|insertAdjacentHTML|document\.write|DOMParser|createContextualFragment/, file);
  }
});

// T-4138: the public chat's stream, language and data table.

test("events are read whole, a split one waits for the next chunk and a bad one is skipped", () => {
  const first = events('event: conversation\ndata: {"id":"c1"}\n\nevent: delta\ndata: {"text":"Dob');
  assert.deepEqual(first.events, [{ name: "conversation", data: { id: "c1" } }]);
  const second = events(first.rest + 'rý"}\n\nevent: delta\ndata: {oops\n\n: comment\n\nevent: done\r\ndata: {}\r\n\r\n');
  assert.deepEqual(second.events, [{ name: "delta", data: { text: "Dobrý" } }, { name: "done", data: {} }]);
  assert.equal(second.rest, "");
  assert.deepEqual(events("").events, []);
});

test("a delta keeps its leading space and multi-line data is joined", () => {
  assert.deepEqual(events('event: delta\ndata: {"text":\ndata: " deň"}\n\n').events, [
    { name: "delta", data: { text: " deň" } },
  ]);
});

test("Slovak by default, the host page's language when the widget knows it", () => {
  const known = { sk: {}, en: {}, cs: {} };
  assert.equal(language("", ["sk", "en"], known), "sk");
  assert.equal(language(undefined, [], known), "sk");
  assert.equal(language("en-GB", ["sk", "en"], known), "en");
  assert.equal(language("hu", ["sk", "en"], known), "sk");
  assert.equal(language("", ["en", "cs"], known), "en");
  assert.equal(language("", ["hu"], known), "sk");
  assert.equal(language("", ["fi"], { en: {} }), "en");
});

test("a data table shows at most 50 rows of text and only http(s) links", () => {
  const rows = Array.from({ length: 60 }, (_, i) => [`Stanica ${i}`, i, null]);
  const shown = table({
    type: "AirQualityObserved",
    columns: ["name", "pm10", "observedAt"],
    rows: [["Námestie SNP", { value: 21 }], ...rows, "not a row"],
    dataset: "https://data.dev.joinedcontext.com/dataset/ovzdusie",
    download: "javascript:alert(1)",
  });
  assert.equal(shown.rows.length, 50);
  assert.deepEqual(shown.rows[0], ["Námestie SNP", '{"value":21}', ""]);
  assert.deepEqual(shown.rows[1], ["Stanica 0", "0", ""]);
  assert.equal(shown.caption, "AirQualityObserved");
  assert.equal(shown.dataset, "https://data.dev.joinedcontext.com/dataset/ovzdusie");
  assert.equal(shown.download, null);
  assert.equal(table({ columns: [], rows: [[1]] }), null);
  assert.equal(table(null), null);
});

test("only an http(s) address is a link", () => {
  assert.equal(safeUrl("https://www.banskabystrica.sk/"), "https://www.banskabystrica.sk/");
  for (const bad of ["javascript:alert(1)", "upload:subory/a.md", "/relative", "data:text/html,x", "https://a b", 42, undefined]) {
    assert.equal(safeUrl(bad), null, String(bad));
  }
});

// T-4149: a link is rendered only to an allowlisted host; anything else is its words alone.
const city = (url) => linkHost(url, ["banskabystrica.sk", "data.dev.joinedcontext.com", "data.gov.sk"]);

test("a link off the allowlist is its words alone, never a link", () => {
  assert.deepEqual(
    markdown("[radnica](https://www.banskabystrica.sk/kontakt) [výhra](https://evil.example/claim)", undefined, city),
    [{ tag: "p", children: [
      { tag: "a", href: "https://www.banskabystrica.sk/kontakt", children: ["radnica"] },
      " ",
      "výhra",
    ] }],
  );
});

test("a markdown image is never an image, and off the list not even a link", () => {
  const blocks = markdown("![logo](https://evil.example/pixel.png?u=1)", undefined, city);
  assert.deepEqual(blocks, [{ tag: "p", children: ["!", "logo"] }]);
  assert.ok(!JSON.stringify(blocks).includes("img"));
});

test("without an allowlist no link is rendered", () => {
  assert.deepEqual(markdown("[mesto](https://banskabystrica.sk)"), [{ tag: "p", children: ["mesto"] }]);
});

test("a host matches itself and its subdomains, never a look-alike", () => {
  assert.ok(city("https://banskabystrica.sk"));
  assert.ok(city("https://www.banskabystrica.sk/a?b#c"));
  assert.ok(city("http://mapy.banskabystrica.sk/"));
  assert.ok(city("https://DATA.GOV.SK/dataset/x"));
  for (const bad of [
    "https://banskabystrica.sk.evil.example/",
    "https://evilbanskabystrica.sk/",
    "https://banskabystrica.sk@evil.example/",
    "https://evil.example\\@banskabystrica.sk/",
    "https://evil.example/?u=https://banskabystrica.sk",
    "javascript:alert(1)//banskabystrica.sk",
    "",
  ]) {
    assert.ok(!city(bad), bad);
  }
  assert.ok(!linkHost("https://banskabystrica.sk", []));
  assert.ok(!linkHost("https://banskabystrica.sk", [""]));
});
