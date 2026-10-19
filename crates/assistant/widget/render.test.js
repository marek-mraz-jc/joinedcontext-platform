// T-3325: the widget's Markdown subset and its sources. Run with `node --test crates/assistant/widget`.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const { notice, closeTargets, connector, monogram, sourceCount, badges, chosen, asksLanguage, ordered, markdown, streamable, sources, label, safeUrl, events, language, table, chart, greeting, title, linkHost, followups, restore, remember, stamp, cards, stage, steps, tel, mailto, suggestions, trouble, retryable, said, retry, vizOrigin, vizSpec, mapFirst, lasting, recalled, privacyLines, dataset, view } = require("./render.js");

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
    { n: 1, url: "https://opendata.dev.joinedcontext.com/dataset/helsinki-events", title: "Helsinki events" },
    { n: 2, url: "https://opendata.dev.joinedcontext.com/dataset/helsinki-events" },
    { n: 3, tool: "query_entities", endpoint: "helsinki-events", url: "https://opendata.dev.joinedcontext.com/dataset/helsinki-events", title: "Helsinki events" },
    { n: 4, tool: "query_entities", endpoint: "helsinki-events" },
    { n: 5, url: "javascript:alert(1)" },
  ]);
  assert.deepEqual(list, [
    { url: "https://opendata.dev.joinedcontext.com/dataset/helsinki-events", title: "Helsinki events", live: false,
      endpoint: null, domain: "opendata.dev.joinedcontext.com", numbers: [1, 2, 3], cards: [] },
    { url: null, title: null, live: true, endpoint: "helsinki-events", domain: "", numbers: [4], cards: [] },
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

test("Slovak by default, a ?lang= the deployment offers when the widget knows it", () => {
  const known = { sk: {}, en: {}, cs: {} };
  assert.equal(language("", ["sk", "en"], known), "sk");
  assert.equal(language(undefined, [], known), "sk");
  assert.equal(language("en-GB", ["sk", "en"], known), "en");
  assert.equal(language("hu", ["sk", "en"], known), "sk");
  assert.equal(language("cs", ["sk", "en"], known), "sk", "a ?lang= the deployment does not offer");
  assert.equal(language("", ["en", "cs"], known), "en");
  // T-4281: the first language the deployment lists opens, Slovak or not.
  assert.equal(language("", ["en", "sk"], known), "en");
  assert.equal(language("", ["hu"], known), "sk");
  assert.equal(language("", ["fi"], { en: {} }), "en");
});

test("the visitor's toggle pick, else the browser's language, else the deployment's first (T-4280, T-4343)", () => {
  const known = { sk: {}, en: {}, cs: {} };
  const skEn = ["sk", "en"];
  assert.equal(language("", skEn, known, null), "sk");
  assert.equal(language("", skEn, known, "en"), "en");
  assert.equal(language("", skEn, known, "hu"), "sk");
  assert.equal(language("", skEn, known, undefined), "sk");
  assert.equal(language("en", skEn, known, ""), "en");
  // browser: the first of navigator.languages the deployment offers.
  assert.equal(language("", skEn, known, null, ["en-GB", "sk"], "browser"), "en");
  assert.equal(language("", skEn, known, null, ["de-DE"], "browser"), "sk", "none offered: the deployment's first");
  assert.equal(language("", ["en", "sk"], known, null, ["de-DE"], "browser"), "en");
  assert.equal(language("", skEn, known, null, ["cs-CZ"], "browser"), "sk", "Czech reads Slovak when there is no cs");
  assert.equal(language("", ["cs", "en"], known, null, ["en-US", "cs"], "browser"), "en");
  assert.equal(language("", ["sk", "cs", "en"], known, null, ["cs-CZ"], "browser"), "cs", "cs when the deployment has it");
  assert.equal(language("", skEn, known, null, ["fr", "de", "EN"], "browser"), "en", "the first offered of several");
  assert.equal(language("", skEn, known, null, [], "browser"), "sk");
  assert.equal(language("", skEn, known, null, undefined, "browser"), "sk");
  // A stored pick wins over the browser, ?lang= over both.
  assert.equal(language("", skEn, known, "en", ["sk-SK"], "browser"), "en");
  assert.equal(language("sk", skEn, known, "en", ["en-GB"], "browser"), "sk");
  // fixed: the deployment's first, whatever the browser or an old pick says; ?lang= still names one.
  assert.equal(language("", skEn, known, "en", ["en-GB"], "fixed"), "sk");
  assert.equal(language("en", skEn, known, null, ["sk"], "fixed"), "en");
  // ask opens like browser beneath the chooser.
  assert.equal(language("", skEn, known, null, ["en-GB"], "ask"), "en");
});

test("a data table shows at most 20 rows of text and only http(s) links", () => {
  const rows = Array.from({ length: 60 }, (_, i) => [`Stanica ${i}`, i, null]);
  const shown = table({
    type: "AirQualityObserved",
    columns: ["name", "pm10", "observedAt"],
    rows: [["Námestie SNP", { value: 21 }], ...rows, "not a row"],
    dataset: "https://opendata.dev.joinedcontext.com/dataset/ovzdusie",
    download: "javascript:alert(1)",
    geojson: "data:application/geo+json,{}",
  }, "en");
  assert.equal(shown.rows.length, 20);
  assert.deepEqual(shown.rows[0], ["Námestie SNP", '{"value":21}', ""]);
  assert.deepEqual(shown.rows[1], ["Stanica 0", "0", ""]);
  assert.equal(shown.caption, "Air quality (20)");
  assert.equal(shown.dataset, "https://opendata.dev.joinedcontext.com/dataset/ovzdusie");
  assert.equal(shown.download, null);
  assert.equal(shown.geojson, null);
  const area = "https://api.dev.joinedcontext.com/api/endpoint/s/ogc/features/collections/AdministrativeArea/items/urn%3Angsi-ld%3AAdministrativeArea%3Ab";
  assert.equal(table({ columns: ["name"], rows: [["Badín"]], geojson: area }, "sk").geojson, area);
  assert.equal(table({ columns: [], rows: [[1]] }), null);
  assert.equal(table(null), null);
});

test("a row measured a day before the table's newest is marked out of date, the rest are not (T-4682)", () => {
  const now = Date.parse("2026-10-18T22:00:00Z");
  const shown = table({
    type: "BikeHireDockingStation",
    columns: ["name", "availableBikeNumber", "observedAt"],
    rows: [["Kamppi", 12, "2026-10-18T21:58:00Z"], ["040", 3, "2026-09-30T09:22:00Z"], ["Töölö", 4, null]],
  }, "sk", now);
  assert.ok(!shown.rows[0][2].includes("neaktuálne"));
  assert.match(shown.rows[1][2], / · neaktuálne$/);
  assert.equal(shown.rows[2][2], "");
  const fresh = table({ columns: ["name", "observedAt"], rows: [["a", "2026-10-01T00:00:00Z"]] }, "en", now);
  assert.ok(!fresh.rows[0][1].includes("out of date"), "one old row alone has nothing newer to lag");
});

test("a data table reads as a person reads it, in sk and en (T-4650)", () => {
  const now = Date.parse("2026-10-18T16:45:00Z");
  const data = {
    type: "AirQualityObserved", total: 12,
    columns: ["name", "pm10", "no2", "open", "dateModified", "observedAt"],
    units: [null, "µg/m³", "µg/m³", null, null, null],
    rows: [
      ["Stanica Žilina", 21.5, 12.000, true, "2026-10-01T08:00:00Z", "2026-10-18T16:40:00Z"],
      ["Stanica BB", 9.12345, null, false, null, "2026-10-16T10:05:00Z"],
    ],
  };
  const sk = table(data, "sk", now);
  assert.equal(sk.caption, "Kvalita ovzdušia — prvých 2 z 12");
  assert.deepEqual(sk.columns, ["Názov", "PM10 (µg/m³)", "NO₂ (µg/m³)", "Open", "Zmenené", "Čas merania"]);
  assert.deepEqual(sk.numeric, [false, true, true, false, false, false]);
  assert.equal(sk.rows[0][1], "21,5");
  assert.equal(sk.rows[0][2], "12");
  assert.equal(sk.rows[1][1], "9,1", "a measured value to one decimal (T-4711)");
  assert.equal(sk.rows[1][2], "");
  assert.deepEqual([sk.rows[0][3], sk.rows[1][3]], ["Áno", "Nie"]);
  assert.match(sk.rows[0][5], /^pred 5 min/);
  assert.match(sk.rows[1][5], /^16\. 10\. 2026 \d{2}:\d{2} · neaktuálne$/);
  const en = table(data, "en", now);
  assert.equal(en.caption, "Air quality — first 2 of 12");
  assert.equal(en.columns[5], "Measured");
  assert.equal(en.rows[1][1], "9.1");
  assert.deepEqual([en.rows[0][3], en.rows[1][3]], ["Yes", "No"]);
  assert.match(en.rows[0][5], /^5 min\. ago$/);
  // A key the table does not know is spelt out; a language it does not know reads English.
  assert.equal(table({ type: "BikeHireDockingStation", columns: ["availableBikeNumber"], rows: [[3]] }, "xx").columns[0], "Available bike number");
  assert.equal(table({ type: "BikeHireDockingStation", columns: ["x"], rows: [] }, "sk").caption, "Stanice bicyklov (0)");
  assert.equal(table({ type: "WasteContainer", columns: ["x"], rows: [] }, "sk").caption, "Waste container (0)");
  // Text stays text: a cell that looks like markup is shown as typed.
  assert.equal(table({ columns: ["name"], rows: [["<img src=x onerror=alert(1)>"]] }, "sk").rows[0][0], "<img src=x onerror=alert(1)>");
});

test("only an http(s) address is a link", () => {
  assert.equal(safeUrl("https://www.banskabystrica.sk/"), "https://www.banskabystrica.sk/");
  for (const bad of ["javascript:alert(1)", "upload:subory/a.md", "/relative", "data:text/html,x", "https://a b", 42, undefined]) {
    assert.equal(safeUrl(bad), null, String(bad));
  }
});

// T-4149: a link is rendered only to an allowlisted host; anything else is its words alone.
const city = (url) => linkHost(url, ["banskabystrica.sk", "opendata.dev.joinedcontext.com", "data.gov.sk"]);

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

test("a host matches itself and its www, never a subdomain or a look-alike (T-4654)", () => {
  assert.ok(city("https://banskabystrica.sk"));
  assert.ok(city("https://www.banskabystrica.sk/a?b#c"));
  assert.ok(city("https://DATA.GOV.SK/dataset/x"));
  assert.ok(city("https://www.data.gov.sk/"));
  assert.ok(linkHost("https://banskabystrica.sk/", ["www.banskabystrica.sk"]));
  for (const bad of [
    "https://evil.banskabystrica.sk/",
    "http://mapy.banskabystrica.sk/",
    "https://www.evil.banskabystrica.sk/",
    "https://x.data.dev.joinedcontext.com/",
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

test("a stage is one fixed line, a search and a read name their topic, an unknown one is nothing (T-4155)", () => {
  assert.equal(stage({ stage: "understanding" }, "sk"), "Rozmýšľam nad otázkou…");
  assert.equal(stage({ stage: "searching", topic: "otváracie hodiny knižnice" }, "sk"),
    "Hľadám v zdrojoch: „otváracie hodiny knižnice“…");
  assert.equal(stage({ stage: "searching" }, "en"), "Searching the sources…");
  assert.equal(stage({ stage: "reading", topic: "AirQualityObserved" }, "cs"), "Čtu živá data: „Kvalita ovzduší“…");
  assert.equal(stage({ stage: "answering", topic: "ignored" }, "xx"), "Preparing the answer…");
  assert.equal(stage({ stage: "searching", topic: "x".repeat(100) }, "en").length, "Searching the sources: „“…".length + 60);
  assert.equal(stage({ stage: "__proto__" }, "en"), null);
  assert.equal(stage({ stage: "thinking" }, "en"), null);
  assert.equal(stage(null, "en"), null);
});

test("a contact card links a well-formed phone and e-mail and shows a malformed one as text (T-4155)", () => {
  assert.deepEqual(cards([{ kind: "contact", name: "Klientske centrum",
    phones: ["+421 48 4330 111", "048/4330-111", "javascript:alert(1)", "12 34", ""],
    emails: ["podatelna@example.org", "a@b", "x@example.org?subject=hi"] }]), [{
    kind: "contact", name: "Klientske centrum",
    phones: [
      { text: "+421 48 4330 111", href: "tel:+421484330111" },
      { text: "048/4330-111", href: "tel:0484330111" },
      { text: "javascript:alert(1)", href: null },
      { text: "12 34", href: null },
    ],
    emails: [
      { text: "podatelna@example.org", href: "mailto:podatelna@example.org" },
      { text: "a@b", href: null },
      { text: "x@example.org?subject=hi", href: null },
    ],
  }]);
  assert.equal(tel("<b>0905123456</b>"), null);
  assert.equal(mailto("a@example.org\nb@example.org"), null);
});

test("a place links an OpenStreetMap search and a procedure keeps its steps (T-4155)", () => {
  assert.deepEqual(cards([
    { kind: "place", address: "Československej armády 26, 974 01 Banská Bystrica" },
    { kind: "procedure", steps: ["Vyplňte žiadosť.", " ", "Zaplaťte poplatok."] },
  ]), [
    { kind: "place", address: "Československej armády 26, 974 01 Banská Bystrica",
      href: "https://www.openstreetmap.org/search?query=%C4%8Ceskoslovenskej%20arm%C3%A1dy%2026%2C%20974%2001%20Bansk%C3%A1%20Bystrica" },
    { kind: "procedure", steps: ["Vyplňte žiadosť.", "Zaplaťte poplatok."] },
  ]);
});

test("an empty or unknown card is dropped and a contact without a name keeps it empty (T-4155)", () => {
  assert.deepEqual(cards(undefined), []);
  assert.deepEqual(cards([null, "x", { kind: "contact" }, { kind: "contact", phones: [" "] },
    { kind: "place", address: "" }, { kind: "procedure", steps: [] }, { kind: "html", html: "<b>" }]), []);
  assert.deepEqual(cards([{ kind: "contact", emails: ["a@example.org"] }]),
    [{ kind: "contact", name: "", phones: [], emails: [{ text: "a@example.org", href: "mailto:a@example.org" }] }]);
});

test("a source carries its citations' cards once each, at most three (T-4155)", () => {
  const place = { kind: "place", address: "Námestie SNP 1, 974 01 Banská Bystrica" };
  const steps = (n) => ({ kind: "procedure", steps: [String(n)] });
  const { list } = sources([
    { n: 1, url: "https://example.org/a", cards: [place] },
    { n: 2, url: "https://example.org/a", cards: [place, steps(1), steps(2), steps(3)] },
    { n: 3, url: "https://example.org/b" },
  ]);
  assert.deepEqual(list[0].cards.map((c) => c.kind), ["place", "procedure", "procedure"]);
  assert.deepEqual(list[1].cards, []);
});

// T-4174: the followups event and the turns the panel keeps in its sessionStorage.
test("a followups event gives at most three short, distinct questions", () => {
  assert.deepEqual(followups({ questions: ["  Kde je  zberný dvor? ", "Kde je zberný dvor?", 7, "", "x".repeat(121), "A b?", "C d?", "E f?"] }),
    ["Kde je zberný dvor?", "A b?", "C d?"]);
  for (const bad of [null, {}, { questions: "a" }, []]) assert.deepEqual(followups(bad), []);
});

test("a kept conversation is read back only in its own shape", () => {
  const id = "6f1c0e9e-3b1a-4d7e-9b51-2c4f8f2a7c11";
  let kept = null;
  for (let n = 1; n <= 22; n++) {
    kept = remember(kept, id, n, { question: "q" + n, text: "a" + n, turn: n, citations: [] });
  }
  assert.equal(kept.turns.length, 20, "the oldest are dropped");
  assert.equal(kept.turns[0].question, "q3");
  const back = restore(JSON.stringify(kept));
  assert.deepEqual(back, kept);
  // Another conversation starts its own list.
  assert.deepEqual(remember(kept, "00000000-0000-4000-8000-000000000000", 1,
    { question: "n", text: "m", turn: 1, citations: [] }).turns.length, 1);
  for (const raw of [null, "", "not json", "[]", '{"conversation":"x","turns":[]}',
    JSON.stringify({ conversation: id, turns: "no" })]) {
    assert.equal(restore(raw), null, String(raw));
  }
  // A turn not of its shape is left out; asked never trails a kept turn.
  const mixed = restore(JSON.stringify({ conversation: id, asked: 1, turns: [
    { question: "q", text: "a", turn: 3, citations: [] }, { question: 1, text: "a", turn: 1, citations: [] },
    { question: "q", text: "a", turn: 0, citations: [] }, { question: "q", text: "<b>x</b>", turn: 4 }] }));
  assert.equal(mixed.turns.length, 1);
  assert.equal(mixed.asked, 3);
});

// T-4198: one numeric column beside a time or a name is drawn as a small chart, nothing else is.
test("a number over time is a line in time order, a number per name is bars", () => {
  const line = chart({ columns: ["dateObserved", "pm10"], rows: [["2026-10-15T10:00:00Z", 21], ["2026-10-15T09:00:00Z", 18.5]] });
  assert.equal(line.kind, "line");
  assert.deepEqual(line.points, [{ label: "2026-10-15T09:00:00Z", value: 18.5 }, { label: "2026-10-15T10:00:00Z", value: 21 }]);
  assert.equal(line.value, "pm10");
  const bars = chart({ columns: ["name", "pm10", "address"], rows: [["Námestie SNP", 21, "x"], ["Zvolenská", 12, "y"]] });
  assert.equal(bars.kind, "bar");
  assert.equal(bars.by, "name");
  assert.deepEqual(bars.points.map((p) => p.value), [21, 12]);
});

test("two numeric columns, text only, one point or no columns draw no chart", () => {
  assert.equal(chart({ columns: ["name", "pm10", "pm25"], rows: [["a", 1, 2], ["b", 3, 4]] }), null);
  assert.equal(chart({ columns: ["name", "street"], rows: [["a", "x"], ["b", "y"]] }), null);
  assert.equal(chart({ columns: ["pm10"], rows: [[1], [2]] }), null);
  assert.equal(chart({ columns: ["name", "pm10"], rows: [["a", 1]] }), null);
  assert.equal(chart({ columns: [], rows: [] }), null);
  assert.equal(chart(null), null);
});

test("at most 50 points, and a value of NaN, a string or nothing is skipped", () => {
  const rows = Array.from({ length: 51 }, (_, i) => [`s${i}`, i]);
  assert.equal(chart({ columns: ["name", "v"], rows }).points.length, 50);
  const skipped = chart({ columns: ["name", "v"], rows: [["a", 1], ["b", NaN], ["c", "7"], ["d", null], ["e", 2], "not a row"] });
  assert.deepEqual(skipped.points.map((p) => p.label), ["a", "e"]);
});

// T-4199: the deployment's greeting is in its first language; another language gets the widget's.
test("the deployment's greeting in its first language, the widget's own in any other", () => {
  assert.equal(greeting("Dobrý deň!", ["sk", "en"], "sk", "Hello!"), "Dobrý deň!");
  assert.equal(greeting("Dobrý deň!", ["sk", "en"], "en", "Hello!"), "Hello!");
  assert.equal(greeting("Hi", undefined, "sk", "Dobrý deň!"), "Hi");
  assert.equal(greeting("", ["sk"], "en", "Hello!"), "");
});

test("an empty chat offers the deployment's questions in its language, else the widget's (T-4253)", () => {
  const own = { sk: ["Kedy je zber odpadu?", " Kedy je zber odpadu? ", "", 7], en: [] };
  const mine = ["a", "b", "c", "d", "e"];
  assert.deepEqual(suggestions(own, "sk", mine), ["Kedy je zber odpadu?"]);
  // English has none of its own: the widget's English, never the Slovak ones.
  assert.deepEqual(suggestions(own, "en", mine), ["a", "b", "c", "d"]);
  assert.deepEqual(suggestions(null, "cs", ["x"]), ["x"]);
  assert.deepEqual(suggestions(undefined, "sk", undefined), []);
});

test("a failed answer is said in the widget's words, by the service's title, else its status (T-4253)", () => {
  assert.equal(trouble(429, "Budget Spent"), "busy");
  // T-4671: each limit is one word, and asking again before it resets cannot help.
  for (const title of ["Daily Limit Reached", "Monthly Limit Reached", "Visitor Limit Reached"]) {
    assert.equal(trouble(429, title), "limit", title);
  }
  assert.equal(retryable("limit"), false);
  assert.equal(trouble(429, "Conversation Budget Spent"), "full");
  assert.equal(trouble(503, "Model Unavailable"), "unreachable");
  assert.equal(trouble(502, "No Answer"), "unanswered");
  assert.equal(trouble(504, "Answer Took Too Long"), "slow");
  assert.equal(trouble(422, "Declined"), "declined", "a provider safety stop (T-4370)");
  assert.equal(trouble(429, "Too Many Requests"), "busy");
  assert.equal(trouble(503), "paused");
  assert.equal(trouble(502), "failed");
  assert.equal(trouble(400, "constructor"), "failed", "a title is looked up as data, never as a property");
  assert.equal(trouble(undefined), "offline");
  assert.equal(trouble(0), "offline");
  assert.equal(retryable("full"), false);
  assert.equal(retryable("declined"), false, "asking the same again is declined again");
  for (const word of ["busy", "paused", "offline", "failed", "unreachable", "unanswered", "slow"]) {
    assert.equal(retryable(word), true, word);
  }
});

test("every word trouble names is in each of the widget's languages (T-4253)", () => {
  const fs = require("node:fs");
  const path = require("node:path");
  const widget = fs.readFileSync(path.join(__dirname, "widget.js"), "utf8");
  const words = ["full", "declined", "unreachable", "unanswered", "slow", "busy", "paused", "offline", "failed", "retry", "limit"];
  for (const lang of ["sk", "en", "cs"]) {
    const block = widget.split(new RegExp(`\\n    ${lang}: \\{`))[1].split(/\n    [a-z]{2}: \{|\n  \};/)[0];
    for (const word of words) assert.match(block, new RegExp(`\\b${word}: "`), `${lang}.${word}`);
    // T-4283: the chips are the deployment's own; the widget invents no city questions.
    assert.doesNotMatch(block, /ideas: \[/, `${lang}.ideas`);
  }
});

test("Retry resends the last question with the history before it, and nothing else (T-4253)", () => {
  const history = [
    { role: "user", text: "Kde parkovať?" },
    { role: "assistant", text: "Na Námestí SNP." },
    { role: "user", text: "A v nedeľu?" },
  ];
  assert.deepEqual(retry(history), { question: "A v nedeľu?", history: history.slice(0, 2) });
  assert.equal(history.length, 3, "the history itself is left alone");
  assert.equal(retry(history.slice(0, 2)), null, "an answered turn is not resent");
  assert.equal(retry([]), null);
  assert.equal(retry(undefined), null);
  assert.equal(retry([{ role: "user", text: "  " }]), null);
});

test("the header's monogram is the first letters of the first two words (T-4283)", () => {
  assert.equal(monogram("Banská Bystrica"), "BB");
  assert.equal(monogram("helsinki"), "H");
  assert.equal(monogram("  Žilina – mesto a univerzita "), "ŽM");
  assert.equal(monogram("1. obvod Praha"), "OP");
  assert.equal(monogram(""), "");
  assert.equal(monogram(undefined), "");
});

test("the panel asks for the language once, only in ask mode and where there is a choice (T-4283, T-4343)", () => {
  const two = ["sk", "en"];
  const ask = (at) => asksLanguage(Object.assign({ mode: "ask" }, at));
  assert.equal(ask({ offered: two }), true, "a first open");
  assert.equal(ask({ offered: two, requested: "en" }), true, "the host page's language does not choose for the visitor");
  assert.equal(ask({ offered: two, stored: "en" }), false, "a later open");
  assert.equal(ask({ offered: two, page: true, requested: "en" }), false, "the full page with ?lang=");
  assert.equal(ask({ offered: two, page: true }), true);
  assert.equal(ask({ offered: ["sk"] }), false, "nothing to choose");
  assert.equal(ask({ offered: two, kept: true }), false, "a conversation already open");
  assert.equal(ask({ offered: two, enabled: false }), false, "a paused assistant");
  assert.equal(asksLanguage(undefined), false);
  // browser (the default) and fixed never ask.
  assert.equal(asksLanguage({ offered: two }), false, "no mode is browser");
  assert.equal(asksLanguage({ offered: two, mode: "browser" }), false);
  assert.equal(asksLanguage({ offered: two, mode: "fixed" }), false);
  assert.equal(chosen("en", two), "en");
  assert.equal(chosen("cs", two), null, "a language no longer offered is asked again");
  assert.equal(chosen(null, two), null);
  assert.deepEqual(ordered(["en", "cs", "sk"]), ["sk", "en", "cs"]);
  assert.deepEqual(ordered(["en"]), ["en"]);
  assert.deepEqual(ordered(undefined), []);
});

// T-4278: the owner's transcript repeated "[1],[2]" after every weekday of the Client Centre's hours.
test("a list whose lines cite the same sources carries the markers once, on its last line", () => {
  const known = (n) => n === 1 || n === 2 || n === 3;
  const cites = (block) => block.children.map((li) => li.children.filter((node) => node.tag === "cite").map((c) => c.numbers));
  const [hours] = markdown("- Monday: 8:00–17:00 [1],[2]\n- Tuesday: 8:00–15:00 [1], [2]\n- Friday: 8:00–13:00 [2][1]", known);
  assert.deepEqual(cites(hours), [[], [], [[2], [1]]]);
  assert.deepEqual(hours.children[0].children, ["Monday: 8:00–17:00"], "no comma or space left where the markers were");
  // A line with other sources keeps its own, and so does the line before it.
  const [mixed] = markdown("1. Roads [3]\n2. Schools [1]\n3. Health [1]", known);
  assert.deepEqual(cites(mixed), [[[3]], [], [[1]]]);
  // Lines that cite nothing, and a paragraph, are left as they are.
  assert.deepEqual(markdown("- a\n- b", known)[0].children.map((li) => li.children), [["a"], ["b"]]);
  assert.deepEqual(cites({ children: markdown("One [1].\nTwo [1].", known) }), [[[1], [1]]]);
});

// T-4278: on an English page the owner read "Vy" and "Užitočná odpoveď" (speakers and thumbs
// named once, in the language of the moment), and a language switch wrote the word over the thumb.
test("the speakers, the thumbs and every answer's tools are named by bound words", () => {
  const fs = require("node:fs");
  const path = require("node:path");
  const widget = fs.readFileSync(path.join(__dirname, "widget.js"), "utf8");
  assert.doesNotMatch(widget, /turn\("jc-(you|bot)", w\./, "a speaker is a key, not the word of the moment");
  assert.doesNotMatch(widget, /setAttribute\("aria-label", w\.(up|down|sources|more)\)/);
  assert.doesNotMatch(widget, /button\("jc-thumb"/, "a thumb's word is its name, never its text");
  assert.match(widget, /words\(b, key, "ariaLabel"\)/);
  // T-4283: copy, thumbs and the header's actions are icon buttons named by their bound word.
  assert.match(widget, /iconButton\("jc-copy", "copy", "copy"\)/);
});

test("the data loop's stages name their data in words, never a slug, and two or more work steps are listed with their times (T-4294, T-4682)", () => {
  assert.equal(stage({ stage: "history", topic: "AirQualityObserved" }, "sk"), "Čítam históriu údajov: „Kvalita ovzdušia“…");
  assert.equal(stage({ stage: "schema", topic: "bbsk-otvorene-data" }, "en"), "Reading the data's schema…");
  assert.equal(stage({ stage: "reading", topic: "WasteContainer" }, "en"), "Reading live data…");
  assert.equal(stage({ stage: "searching", topic: "cyklotrasy v BB" }, "sk"), "Hľadám v zdrojoch: „cyklotrasy v BB“…");
  assert.equal(stage({ stage: "steps" }, "en"), null, "the summary word is no stage");
  const took = steps([{ stage: "understanding" }, { stage: "reading", topic: "School", ms: 1240 }, { stage: "answering" },
    { stage: "schema", topic: "od", ms: -5 }, { stage: "bogus" }], "sk");
  assert.deepEqual(took, { summary: "Postup (2)", lines: ["Čítam živé údaje: „Školy“… (1,2 s)", "Čítam schému údajov…"] });
  assert.equal(steps([{ stage: "reading", topic: "School" }], "en"), null, "one step needs no list");
  assert.equal(steps(undefined, "en"), null);
});

test("a message's time is the visitor's HH:MM with the UTC minute beside it, kept across a page change (T-4339)", () => {
  const at = stamp("2026-10-16T12:05:59.123Z", "sk");
  assert.equal(at.datetime, "2026-10-16T12:05Z");
  assert.equal(at.title, "2026-10-16 12:05 UTC");
  const local = new Date("2026-10-16T12:05:59Z");
  assert.match(at.text, new RegExp("^0?" + local.getHours() + "[:.]" + String(local.getMinutes()).padStart(2, "0")));
  assert.equal(stamp(Date.UTC(2026, 9, 16, 23, 59)).datetime, "2026-10-16T23:59Z");
  // A 24-hour clock in English too, never "02:17 PM".
  assert.doesNotMatch(stamp("2026-10-16T12:17:00Z", "en").text, /AM|PM/);
  for (const none of [null, undefined, "", "yesterday", NaN]) assert.equal(stamp(none), null);
  const conversation = "0b8e7e8e-3f7a-4b9e-9a51-1d2c3b4a5f60";
  const kept = remember(null, conversation, 1, { question: "q", text: "a", turn: 1, citations: [],
    askedAt: "2026-10-16T12:05:01Z", at: "2026-10-16T12:05:09Z" });
  const back = restore(JSON.stringify(kept));
  assert.equal(back.turns[0].askedAt, "2026-10-16T12:05Z");
  assert.equal(back.turns[0].at, "2026-10-16T12:05Z");
  // A planted or broken time is dropped, the turn still shown.
  const planted = restore(JSON.stringify({ conversation, asked: 1, turns: [
    { question: "q", text: "a", turn: 1, citations: [], askedAt: "<img src=x>", at: 7 } ] }));
  assert.equal(planted.turns[0].askedAt, undefined);
  assert.equal(planted.turns[0].at, undefined);
  assert.equal(planted.turns[0].text, "a");
});

// T-4365: a greeting and a title per language.
test("a greeting per language is this language's, else the widget's own", () => {
  const own = { sk: "Dobrý deň!", en: "Hello! Ask about the catalogue." };
  assert.equal(greeting(own, ["sk", "en", "fi"], "en", "Hello!"), "Hello! Ask about the catalogue.");
  assert.equal(greeting(own, ["sk", "en", "fi"], "fi", "Hei!"), "Hei!");
  assert.equal(greeting({ sk: "  " }, ["sk"], "sk", "Dobrý deň!"), "Dobrý deň!");
  assert.equal(greeting(own, ["sk"], "constructor", "Hi"), "Hi");
});

test("a title per language is this language's, else the first language's, else any", () => {
  assert.equal(title(" data.dev ", ["sk", "en"], "en"), "data.dev");
  assert.equal(title({ sk: "Mesto", en: "City" }, ["sk", "en"], "en"), "City");
  assert.equal(title({ sk: "Mesto", en: "City" }, ["sk", "en", "fi"], "fi"), "Mesto");
  assert.equal(title({ en: "City" }, ["sk", "en"], "sk"), "City");
  assert.equal(title({}, ["sk"], "sk"), "");
  assert.equal(title(null, ["sk"], "sk"), "");
  assert.equal(title(["Mesto"], ["sk"], "sk"), "");
});

test("the footer's count takes each language's plural for 1, 2 and 5 (T-4364)", () => {
  const forms = (lang) => [1, 2, 5].map((n) => sourceCount(n, lang));
  assert.deepEqual(forms("sk"), ["1 zdroj", "2 zdroje", "5 zdrojov"]);
  assert.deepEqual(forms("cs"), ["1 zdroj", "2 zdroje", "5 zdrojů"]);
  assert.deepEqual(forms("en"), ["1 source", "2 sources", "5 sources"]);
  assert.equal(sourceCount(22, "sk"), "22 zdrojov");
  assert.equal(sourceCount(3, "de"), "3 sources");
});

test("at most three badges, one per site, a letter and a hue that the site alone decides (T-4364)", () => {
  const { list } = sources([
    { n: 1, url: "https://www.zilina.sk/a" }, { n: 2, url: "https://zilina.sk/b" },
    { n: 3, url: "https://3dzilina.sk/" }, { n: 4, tool: "query_entities", endpoint: "parkovanie" },
    { n: 5, url: "https://kia.sk/" },
  ]);
  const got = badges(list);
  assert.deepEqual(got.map((b) => [b.site, b.letter]), [["zilina.sk", "Z"], ["3dzilina.sk", "3"], ["parkovanie", "P"]]);
  assert.equal(got[0].hue, badges([{ domain: "zilina.sk" }])[0].hue);
  assert.ok(got.every((b) => Number.isInteger(b.hue) && b.hue >= 0 && b.hue < 360));
  assert.deepEqual(badges([]), []);
  assert.deepEqual(badges(undefined), []);
});

test("opening hours read as a list of bold days and their times, not one paragraph (T-4364)", () => {
  assert.deepEqual(markdown("Klientske centrum má otvorené:\n- **Pondelok:** 8:00 – 17:00\n- **Piatok:** 8:00 – 12:00 [1]", upTo(1)), [
    { tag: "p", children: ["Klientske centrum má otvorené:"] },
    { tag: "ul", children: [
      { tag: "li", children: [{ tag: "strong", children: ["Pondelok:"] }, " 8:00 – 17:00"] },
      { tag: "li", children: [{ tag: "strong", children: ["Piatok:"] }, " 8:00 – 12:00 ", { tag: "cite", numbers: [1] }] },
    ] },
  ]);
});

test("the visitor proof finds a nonce whose hash starts with the bits asked (T-4653)", async () => {
  const { solve, zeroBits } = require("./visitor.js");
  const { createHash } = require("node:crypto");
  assert.equal(zeroBits(Uint8Array.from([0, 0x0f, 0xff])), 12);
  assert.equal(zeroBits(Uint8Array.from([0x80])), 0);
  assert.equal(zeroBits(Uint8Array.from([0, 0])), 16);
  const challenge = "3a1f.1760792400.10.9f2c";
  const nonce = await solve(challenge, 10);
  assert.match(nonce, /^\d+$/);
  assert.ok(zeroBits(createHash("sha256").update(`${challenge}:${nonce}`).digest()) >= 10);
});

test("a grid reads by meaning: no URN column, years and codes ungrouped, enums in words, short links (T-4682)", () => {
  const data = {
    type: "Bridge", total: 1651,
    columns: ["id", "bridgedLength", "yearBuilt", "stationCode", "heritageStatus", "refRoad", "source", "spanCount"],
    units: [null, "m", null, null, null, null, null, null],
    enums: [null, null, null, null, { notListed: { sk: "nie je chránená", en: "not protected" } }, null, null, null],
    rows: [
      ["urn:ngsi-ld:Bridge:bbsk.sk:bbsk-registre:M1006", 1651.5, 1953, 12345, "notListed", "urn:ngsi-ld:Road:bbsk.sk:I-50", "https://data.slovensko.sk/download?id=9", null],
      ["urn:ngsi-ld:Bridge:bbsk.sk:bbsk-registre:M1007", 9.46, 2021, 7, "other", null, "javascript:alert(1)", 3],
    ],
  };
  const sk = table(data, "sk");
  // The URN is a name, never a column, and a column of references goes.
  assert.deepEqual(sk.columns.length, 7);
  assert.equal(sk.columns[0], "Názov");
  assert.equal(sk.rows[0][0], "M1006");
  assert.ok(!JSON.stringify(sk).includes("urn:"), JSON.stringify(sk));
  // A quantity is grouped; a year or a code is not.
  assert.equal(sk.rows[0][1], "1\u00a0651,5".replace("\u00a0", new Intl.NumberFormat("sk").format(1000).charAt(1)));
  assert.equal(sk.rows[0][2], "1953");
  assert.equal(sk.rows[0][3], "12345");
  assert.equal(sk.rows[1][1], "9,5", "a length in metres to one decimal (T-4711)");
  // An enum reads its words; one the model does not describe stays as written.
  assert.equal(sk.rows[0][4], "nie je chránená");
  assert.equal(table(data, "en").rows[0][4], "not protected");
  assert.equal(sk.rows[1][4], "other");
  // A long address is a short link; anything but http(s) stays text.
  assert.deepEqual(sk.rows[0][5], { text: "Zdroj", href: "https://data.slovensko.sk/download?id=9" });
  assert.equal(sk.rows[1][5], "javascript:alert(1)");
  // An empty cell is empty.
  assert.equal(sk.rows[0][6], "");
  assert.equal(sk.caption, "Mosty — prvých 2 z 1\u00a0651".replace("\u00a0", new Intl.NumberFormat("sk").format(1000).charAt(1)));
  assert.equal(table(data, "en").caption, "Bridges — first 2 of 1,651");
});

test("the table's labels are jc-core's field-order.json, the CKAN publisher's and theme's (T-4661)", () => {
  const fields = require("../../jc-core/data/field-order.json");
  const shown = table({ columns: ["startDate", "eventCategory", "url"], rows: [] }, "sk");
  assert.deepEqual(shown.columns, ["Začiatok", "Kategória", "Odkaz"]);
  // `id` is the row's name in the grid (T-4682), so it reads as `name` does.
  for (const key of Object.keys(fields.labels).filter((k) => k !== "id")) {
    assert.equal(table({ columns: [key], rows: [] }, "cs").columns[0], fields.labels[key].cs, key);
  }
});

test("the data model's labels win where the table carries them, as text (T-4672)", () => {
  const shown = table({
    columns: ["name", "pm10", "url"],
    units: [null, "µg/m³", null],
    labels: [{ sk: "Názov stanice", en: "Station" }, { en: "PM10 <b>dust</b>" }, null],
    rows: [["A", 21.5, "https://x.sk"]]
  }, "sk");
  // sk where the model has it, else its English, else the shared table's.
  assert.deepEqual(shown.columns, ["Názov stanice", "PM10 <b>dust</b> (µg/m³)", "Odkaz"]);
  assert.deepEqual(table({ columns: ["url"], labels: "nope", rows: [] }, "sk").columns, ["Odkaz"]);
});

test("a pipe table becomes rows, its rule making the first one the header (T-4649)", () => {
  assert.deepEqual(markdown("| Stanica | NO₂ |\n|:---|---:|\n| **Centrum** | 12 \\| 14 |\nend"), [
    { tag: "table", children: [
      { tag: "tr", children: [{ tag: "th", children: ["Stanica"] }, { tag: "th", children: ["NO₂"] }] },
      { tag: "tr", children: [{ tag: "td", children: [{ tag: "strong", children: ["Centrum"] }] }, { tag: "td", children: ["12 | 14"] }] },
    ] },
    { tag: "p", children: ["end"] },
  ]);
  assert.deepEqual(markdown("a | b"), [{ tag: "p", children: ["a | b"] }], "a pipe inside a sentence stays text");
});

test("a fenced block keeps its lines as code, markers and all", () => {
  assert.deepEqual(markdown("```js\nconst a = \"**x**\";\n\n- y\n```\nafter"), [
    { tag: "pre", children: [{ tag: "code", children: ["const a = \"**x**\";\n\n- y"] }] },
    { tag: "p", children: ["after"] },
  ]);
});

// Every text the tree would show, as one string.
const shown = (nodes) => nodes.map((n) => (typeof n === "string" ? n : shown(n.children || []))).join("\n");

const STREAMED = [
  "## Doprava",
  "",
  "Máme **tri** zdroje [1, 2]:",
  "",
  "- **`type`**: typ entity [1]",
  "- **`q`**: filter [2, 5]",
  "",
  "| Zdroj | Stav |",
  "|---|---|",
  "| **Parkoviská** | `live` |",
  "",
  "```",
  "GET /ngsi-ld/v1/entities?type=OffStreetParking",
  "```",
  "Hotovo [docs](https://example.sk/a).",
  "",
].join("\n");

test("no prefix of a streamed answer shows a marker, and the end equals the final render", () => {
  const cited = upTo(5);
  const linkable = (url) => linkHost(url, ["example.sk"]);
  for (let at = 1; at <= STREAMED.length; at += 1) {
    const tree = markdown(streamable(STREAMED.slice(0, at)), null, linkable);
    const text = shown(tree.filter((block) => block.tag !== "pre"));
    assert.ok(!/\*\*|`|^\s*\||^#|\[\d|\]\(/m.test(text), `prefix ${at} shows a marker: ${JSON.stringify(text)}`);
  }
  assert.deepEqual(markdown(streamable(STREAMED), cited, linkable), markdown(STREAMED, cited, linkable));
});

test("a token split across deltas waits for its close", () => {
  assert.equal(streamable("Hello **bo"), "Hello bo");
  assert.deepEqual(markdown(streamable("Hello **bo" + "ld**")), [{ tag: "p", children: ["Hello ", { tag: "strong", children: ["bold"] }] }]);
  assert.equal(streamable("text\n| a | b"), "text\n", "half a table row waits for its line");
  assert.equal(streamable("```py\nprint(1)"), "```py\nprint(1)\n```", "an open fence shows its code so far");
  assert.equal(streamable("see [1, 2"), "see ", "half a citation shows nothing");
  assert.equal(streamable("read [the page](https://exa"), "read the page");
  assert.equal(streamable("- **`ty"), "- ty");
  assert.equal(streamable("text\n##"), "text\n");
});

test("HTML in an answer, streamed or not, stays text", () => {
  for (const text of ["<script>alert(1)</script>", "<img src=x onerror=alert(1)>", "| <b>x</b> |"]) {
    const tree = markdown(streamable(text + "\n"));
    const tags = JSON.stringify(tree).match(/"tag":"[a-z]+"/g) || [];
    assert.ok(tags.every((t) => /"(p|table|tr|td)"/.test(t)), JSON.stringify(tree));
    assert.ok(shown(tree).includes("<"), "the angle brackets are shown as typed");
  }
});

test("a chart or a map reaches only the viz origin, in the visitor's language, map first when every row has a place (T-4657)", () => {
  assert.equal(vizOrigin("https://viz.dev.joinedcontext.com"), "https://viz.dev.joinedcontext.com");
  for (const bad of ["http://viz.dev.joinedcontext.com", "https://viz.dev.joinedcontext.com/", "https://evil.example", "javascript:alert(1)", null, 7]) {
    assert.equal(vizOrigin(bad), null, String(bad));
  }
  const map = { $schema: "urn:joinedcontext:map:v1", label: "name", popup: [{ field: "pm10", label: "pm10", unit: "µg/m³" }, { field: "temperature", label: "temperature" }],
    data: { type: "FeatureCollection", features: [{}, {}] } };
  const shown = vizSpec(map, "sk");
  assert.deepEqual(shown.popup.map((f) => f.label), ["PM10", "Teplota"]);
  assert.equal(map.popup[0].label, "pm10", "the event's spec stays as it came");
  const bar = { encoding: { x: { field: "value", title: "no2 (µg/m³)" }, y: { field: "label", title: null } } };
  assert.equal(vizSpec(bar, "en").encoding.x.title, "NO₂ (µg/m³)");
  assert.equal(vizSpec(bar, "en").encoding.y.title, null);
  // T-4682: the model's words for the column win, in the visitor's language.
  assert.equal(vizSpec(bar, "sk", { no2: { sk: "Oxid dusičitý", en: "Nitrogen dioxide" } }).encoding.x.title, "Oxid dusičitý (µg/m³)");
  assert.equal(vizSpec(bar, "en", { no2: "nope" }).encoding.x.title, "NO₂ (µg/m³)");
  assert.equal(mapFirst({ rows: [[1], [2]], map }), true);
  assert.equal(mapFirst({ rows: [[1], [2], [3]], map }), false, "a row without a place: the table first");
  assert.equal(mapFirst({ rows: [[1]] }), false);
});

test("the close message goes to the framing page's origin alone, never to any (T-4654)", () => {
  const framers = ["https://www.banskabystrica.sk", "https://opendata.dev.joinedcontext.com"];
  assert.deepEqual(closeTargets(["https://www.banskabystrica.sk"], framers), ["https://www.banskabystrica.sk"]);
  // Without the browser's record (Firefox), each origin that may frame it; the others drop it.
  assert.deepEqual(closeTargets(undefined, framers), framers);
  assert.deepEqual(closeTargets([], ["HTTPS://City.SK/", "http://localhost:8080"]), ["https://city.sk", "http://localhost:8080"]);
  assert.deepEqual(closeTargets(undefined, ["*", "", null, "null", "javascript:alert(1)", "https://a.sk/path", "https://a.sk@evil.example"]), []);
  assert.deepEqual(closeTargets(["null"], framers), [], "a sandboxed parent gets nothing");
  assert.deepEqual(closeTargets(undefined, undefined), []);
});

test("a limit reads the service's sentence with its reset time in Slovak, the widget's word elsewhere (T-4671)", () => {
  const words = { limit: "The assistant has reached its limit. Try again later.", slow: "Too long." };
  const day = "Asistent dnes dosiahol denný limit. Skúste to znova zajtra po 2:00.";
  assert.equal(said("limit", day, "sk", words), day);
  assert.equal(said("limit", day, "en", words), words.limit, "never Slovak in an English chat");
  assert.equal(said("limit", "  ", "sk", words), words.limit);
  assert.equal(said("limit", undefined, "sk", words), words.limit);
  assert.equal(said("slow", "Answer Took Too Long detail", "sk", words), words.slow, "only a limit reads the detail");
});

test("a remembered conversation comes back for 90 days after its last change and not after (T-4670)", () => {
  const id = "00000000-0000-4000-8000-000000000000";
  const kept = remember(null, id, 1, { question: "Kedy?", text: "Zajtra.", turn: 1, citations: [] });
  const saved = new Date("2026-10-18T12:00:00Z");
  const raw = lasting(kept, saved);
  assert.equal(recalled(raw, new Date("2027-01-15T12:00:00Z")).kept.turns[0].question, "Kedy?");
  assert.equal(recalled(raw, new Date("2027-01-17T12:00:00Z")), null, "past 90 days");
  // Switched on before the first question: remembered, nothing to show yet.
  assert.deepEqual(recalled(lasting(null, saved), saved), { kept: null });
  for (const bad of [null, "", "{", "[]", '{"kept":{}}', '{"savedAt":"yesterday"}',
    JSON.stringify({ savedAt: "2030-01-01T00:00:00Z", kept })]) {
    assert.equal(recalled(bad, saved), null, String(bad));
  }
  // A planted conversation of the wrong shape is not shown.
  assert.deepEqual(recalled(JSON.stringify({ savedAt: saved.toISOString(), kept: { conversation: "x", turns: [] } }), saved), { kept: null });
});

test("the privacy dialog leads with the deployment's disclaimer, says what is kept, and names only a contact and an https page it was given (T-4670)", () => {
  const words = { aboutAi: "AI", keptHeading: "Kept", keptDays: "kept {days} days", keptNone: "kept nothing", askHeading: "Ask",
    askData: "write to", documents: "documents" };
  assert.deepEqual(privacyLines(words, 30, null), [{ text: "AI" }, { heading: "Kept", text: "kept 30 days" }]);
  assert.deepEqual(privacyLines(words, 0, {}, "  "), [{ text: "AI" }, { heading: "Kept", text: "kept nothing" }]);
  assert.equal(privacyLines(words, 30, null, " The city's own words. ")[0].text, "The city's own words.");
  assert.deepEqual(privacyLines(words, 7, { contact: "dpo@banskabystrica.sk", documentsUrl: "https://www.banskabystrica.sk/gdpr" }), [
    { text: "AI" }, { heading: "Kept", text: "kept 7 days" },
    { heading: "Ask", text: "write to", mail: "dpo@banskabystrica.sk" },
    { text: "documents", href: "https://www.banskabystrica.sk/gdpr" },
  ]);
  assert.equal(privacyLines(words, 7, { contact: "not an address", documentsUrl: "https://x.sk/gdpr" }).length, 2, "no contact, no section");
  assert.equal(privacyLines(words, 7, { contact: "dpo@x.sk", documentsUrl: "javascript:alert(1)" }).length, 3);
  assert.equal(privacyLines(words, 7, { contact: "dpo@x.sk", documentsUrl: "http://x.sk/gdpr" }).length, 3, "https only");
});

test("the connector panel gives each client's steps for an https address and nothing else (T-4694)", () => {
  const url = "https://api.dev.joinedcontext.com/api/mcp/d/bb-doprava";
  const panel = connector(url, "bb-doprava", "en");
  assert.deepEqual(panel.clients.map((c) => c.name), ["Claude", "ChatGPT", "VS Code, Cursor, Claude Code"]);
  const code = panel.clients[2].steps.map((s) => s.code);
  assert.deepEqual(JSON.parse(code[0]), { servers: { "bb-doprava": { type: "http", url } } });
  assert.deepEqual(JSON.parse(code[1]), { mcpServers: { "bb-doprava": { url } } });
  assert.equal(code[2], "claude mcp add --transport http bb-doprava " + url);
  // No step is a link: no client documents a one-click add.
  for (const client of panel.clients) for (const step of client.steps) assert.ok(!("href" in step));
  assert.equal(connector(url, "bb-doprava", "de").title, connector(url, "bb-doprava", "sk").title, "Slovak for a language it lacks");
  assert.equal(connector(url, "x\"; rm -rf /", "en").clients[2].steps[2].code, "claude mcp add --transport http assistant " + url);
  for (const bad of [null, "", "http://api.example/api/mcp/d/x", "javascript:alert(1)", "https://a b/x", "https://api.example/x y"]) {
    assert.equal(connector(bad, "x", "en"), null, String(bad));
  }
});

const C = "6f1c0e9e-3b1a-4d7e-9b51-2c4f8f2a7c11";
const T = "0a1b2c3d-4e5f-4a7b-8c9d-0e1f2a3b4c5d";

test("an answer about one dataset opens it, a broad or an outside one never (T-4685)", () => {
  const cyklo = "https://opendata.dev.example/dataset/cyklotrasy-bbsk";
  // One dataset, cited first, twice and once by a tool: open it, the trailing slash and query dropped.
  assert.deepEqual(dataset([{ n: 1, url: cyklo + "/", title: "Cyklotrasy BBSK" }, { n: 2, url: "https://www.bbsk.sk/doprava" },
    { n: 3, tool: "query_entities", url: cyklo + "?x=1", title: "Cyklotrasy" }]), { url: cyklo, title: "Cyklotrasy BBSK" });
  // Two datasets: a list, stay.
  assert.equal(dataset([{ n: 1, url: cyklo }, { n: 2, url: "https://opendata.dev.example/dataset/zastavky" }]), null);
  // The dataset is not the first citation: the answer is about something else.
  assert.equal(dataset([{ n: 1, url: "https://www.bbsk.sk/doprava" }, { n: 2, url: cyklo }]), null);
  // No citation, a tool without a page, http, a resource of the dataset, or a lookalike path.
  assert.equal(dataset([]), null);
  assert.equal(dataset(null), null);
  assert.equal(dataset([{ n: 1, tool: "query_entities" }]), null);
  assert.equal(dataset([{ n: 1, url: "http://opendata.dev.example/dataset/cyklotrasy-bbsk" }]), null);
  assert.deepEqual(dataset([{ n: 1, url: "http://127.0.0.1:8080/dataset/cyklotrasy-bbsk" }]), { url: "http://127.0.0.1:8080/dataset/cyklotrasy-bbsk", title: "" });
  assert.equal(dataset([{ n: 1, url: cyklo + "/resource/1" }]), null);
  assert.equal(dataset([{ n: 1, url: "https://evil.example/x/dataset/cyklotrasy" }]), null);
  assert.equal(dataset([{ n: 1, url: "javascript:alert(1)//dataset/x" }]), null);
  // A hostile title is cut, never markup: the widget writes it as text.
  assert.equal(dataset([{ n: 1, url: cyklo, title: "<img src=x>".repeat(50) }]).title.length, 200);
});

test("the panel's view comes back after a page change: draft, caret, scroll and a streaming answer (T-4685)", () => {
  const saved = { draft: "Kde sú cyklotrasy?", caret: 4, scroll: 320, focused: true,
    streaming: { turn: T, conversation: C, number: 2, question: "Aké cyklotrasy?", text: "Cyklotrasy sú", askedAt: "2026-10-19T08:00:00Z" } };
  assert.deepEqual(view(JSON.stringify(saved)), { draft: "Kde sú cyklotrasy?", caret: 4, scroll: 320, focused: true,
    streaming: { turn: T, conversation: C, number: 2, question: "Aké cyklotrasy?", text: "Cyklotrasy sú", askedAt: "2026-10-19T08:00Z" } });
  // A caret past the draft is at its end; a bad scroll is none; a turn that is no UUID streams nothing.
  assert.deepEqual(view(JSON.stringify({ draft: "ab", caret: 9, scroll: -1, streaming: { ...saved.streaming, turn: "../x" } })),
    { draft: "ab", caret: 2, scroll: null, focused: false, streaming: null });
  assert.deepEqual(view(JSON.stringify({ draft: 7, focused: "yes" })), { draft: "", caret: 0, scroll: null, focused: false, streaming: null });
  assert.equal(view("not json"), null);
  assert.equal(view(null), null);
  assert.equal(view(JSON.stringify("x")), null);
  // Overlong pieces are cut.
  assert.equal(view(JSON.stringify({ draft: "x".repeat(5000) })).draft.length, 4000);
});

test("a turn that opened a dataset keeps it, a forged one is dropped (T-4685)", () => {
  const turn = { question: "q", text: "a", turn: 1, citations: [] };
  const kept = restore(JSON.stringify({ conversation: C, asked: 1, turns: [
    { ...turn, opened: { url: "https://opendata.dev.example/dataset/cyklotrasy-bbsk", title: "Cyklotrasy" } },
    { ...turn, turn: 2, opened: { url: "javascript:alert(1)", title: "x" } }] }));
  assert.deepEqual(kept.turns[0].opened, { url: "https://opendata.dev.example/dataset/cyklotrasy-bbsk", title: "Cyklotrasy" });
  assert.equal(kept.turns[1].opened, undefined);
});

test("a measured value shows one decimal and a plain number two, in the visitor's locale (T-4711)", () => {
  const data = {
    type: "AirQualityObserved",
    columns: ["name", "pm10", "ratio", "count"],
    units: [null, "µg/m³", null, null],
    rows: [["Žilina, Obežná", 11.506, 0.12345, 42]],
  };
  const sk = table(data, "sk");
  assert.deepEqual(sk.rows[0], ["Žilina, Obežná", "11,5", "0,12", "42"]);
  assert.deepEqual(table(data, "en").rows[0].slice(1), ["11.5", "0.12", "42"]);
});
