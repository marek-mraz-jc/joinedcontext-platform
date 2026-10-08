// T-3325: the widget's Markdown subset and its sources. Run with `node --test crates/assistant/widget`.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const { markdown, sources, label } = require("./render.js");

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

test("an http(s) link is a link; any other scheme stays text", () => {
  assert.deepEqual(markdown("[the city](https://hel.fi/x) [bad](javascript:alert(1)) [ftp](ftp://x)"), [
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
