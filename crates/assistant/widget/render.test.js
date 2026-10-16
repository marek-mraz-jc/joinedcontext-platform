// T-3325: the widget's Markdown subset and its sources. Run with `node --test crates/assistant/widget`.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const { monogram, chosen, asksLanguage, ordered, markdown, sources, label, safeUrl, events, language, table, chart, greeting, linkHost, followups, restore, remember, cards, stage, tel, mailto, suggestions, trouble, retryable, retry } = require("./render.js");

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
      endpoint: null, domain: "data.dev.joinedcontext.com", numbers: [1, 2, 3], cards: [] },
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

test("Slovak by default, the host page's language when the widget knows it", () => {
  const known = { sk: {}, en: {}, cs: {} };
  assert.equal(language("", ["sk", "en"], known), "sk");
  assert.equal(language(undefined, [], known), "sk");
  assert.equal(language("en-GB", ["sk", "en"], known), "en");
  assert.equal(language("hu", ["sk", "en"], known), "sk");
  assert.equal(language("", ["en", "cs"], known), "en");
  // T-4281: the first language the deployment lists opens, Slovak or not.
  assert.equal(language("", ["en", "sk"], known), "en");
  assert.equal(language("", ["hu"], known), "sk");
  assert.equal(language("", ["fi"], { en: {} }), "en");
});

// T-4280: the visitor's choice in the widget wins; without one the chat opens in Slovak, and only
// a site's deliberate data-lang (never the host page's lang, which no longer reaches the widget)
// changes that.
test("the visitor's stored choice first, else Slovak", () => {
  const known = { sk: {}, en: {}, cs: {} };
  assert.equal(language("", ["sk", "en"], known, null), "sk");
  assert.equal(language("", ["sk", "en"], known, "en"), "en");
  assert.equal(language("cs", ["sk", "en"], known, "en"), "en");
  assert.equal(language("", ["sk", "en"], known, "hu"), "sk");
  assert.equal(language("", ["sk", "en"], known, undefined), "sk");
  assert.equal(language("en", ["sk", "en"], known, ""), "en");
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

test("a stage is one fixed line, a search and a read name their topic, an unknown one is nothing (T-4155)", () => {
  assert.equal(stage({ stage: "understanding" }, "sk"), "Rozmýšľam nad otázkou…");
  assert.equal(stage({ stage: "searching", topic: "otváracie hodiny knižnice" }, "sk"),
    "Hľadám v zdrojoch: „otváracie hodiny knižnice“…");
  assert.equal(stage({ stage: "searching" }, "en"), "Searching the sources…");
  assert.equal(stage({ stage: "reading", topic: "AirQualityObserved" }, "cs"), "Čtu živá data: „AirQualityObserved“…");
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
  // T-4286: the service never sends a spent day as an error, so the widget has no word for it.
  assert.equal(trouble(429, "Budget Spent"), "busy");
  assert.equal(trouble(429, "Conversation Budget Spent"), "full");
  assert.equal(trouble(503, "Model Unavailable"), "unreachable");
  assert.equal(trouble(502, "No Answer"), "unanswered");
  assert.equal(trouble(504, "Answer Took Too Long"), "slow");
  assert.equal(trouble(429, "Too Many Requests"), "busy");
  assert.equal(trouble(503), "paused");
  assert.equal(trouble(502), "failed");
  assert.equal(trouble(400, "constructor"), "failed", "a title is looked up as data, never as a property");
  assert.equal(trouble(undefined), "offline");
  assert.equal(trouble(0), "offline");
  assert.equal(retryable("full"), false);
  for (const word of ["busy", "paused", "offline", "failed", "unreachable", "unanswered", "slow"]) {
    assert.equal(retryable(word), true, word);
  }
});

test("every word trouble names is in each of the widget's languages (T-4253)", () => {
  const fs = require("node:fs");
  const path = require("node:path");
  const widget = fs.readFileSync(path.join(__dirname, "widget.js"), "utf8");
  const words = ["full", "unreachable", "unanswered", "slow", "busy", "paused", "offline", "failed", "retry"];
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

test("the panel asks for the language once, and only where there is a choice (T-4283)", () => {
  const two = ["sk", "en"];
  assert.equal(asksLanguage({ offered: two }), true, "a first open");
  assert.equal(asksLanguage({ offered: two, requested: "en" }), true, "the host page's language does not choose for the visitor");
  assert.equal(asksLanguage({ offered: two, stored: "en" }), false, "a later open");
  assert.equal(asksLanguage({ offered: two, page: true, requested: "en" }), false, "the full page with ?lang=");
  assert.equal(asksLanguage({ offered: two, page: true }), true);
  assert.equal(asksLanguage({ offered: ["sk"] }), false, "nothing to choose");
  assert.equal(asksLanguage({ offered: two, kept: true }), false, "a conversation already open");
  assert.equal(asksLanguage({ offered: two, enabled: false }), false, "a paused assistant");
  assert.equal(asksLanguage(undefined), false);
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
