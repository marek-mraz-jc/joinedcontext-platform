// T-4283: the widget's layout in a browser, at a desktop, a tablet and a phone. A small server
// stands in for jc-assistant: the page and its script as src/chat/widget.rs serves them, a chat
// route that streams one long answer, and a host page with the launcher (embed.js).
//   cd crates/assistant/widget && npm i --no-save @playwright/test@1.62.1 && npx playwright test -c layout.config.mjs
import { createServer } from "node:http";
import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "@playwright/test";

const here = dirname(fileURLToPath(import.meta.url));
const read = (name) => readFileSync(join(here, name), "utf8");
const CONFIG = {
  publicId: "bb", title: "Banská Bystrica", color: "#1f5fae", languages: ["sk", "en"], linkHosts: ["example.sk"],
  greeting: "Dobrý deň! Opýtajte sa ma na služby mesta a kraja.",
  suggestions: { sk: ["Kedy má otvorené klientske centrum?", "Ako zaplatím parkovanie?", "Aká je kvalita ovzdušia?"],
    en: ["When is the client centre open?", "How do I pay for parking?", "What is the air quality?"] },
  disclaimer: { sk: "Odpovede tvorí umelá inteligencia z webov mesta a kraja. Môžu obsahovať chyby, overte si ich v zdrojoch. Nezadávajte osobné údaje.",
    en: "Answers are written by AI from the city's and the region's websites. They may contain mistakes; check them in the sources. Do not enter personal data." },
  enabled: true, connectors: [],
  transcriptDays: 30, privacy: { contact: "dpo@banskabystrica.sk", documentsUrl: "https://www.banskabystrica.sk/gdpr" },
};
const attr = (text) => text.replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
// The deployment's `widget` the next page is served with (T-4343); none is `browser`, toggle on.
let WIDGET;
const PAGE = () => `<!doctype html><html lang="sk"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><link rel="stylesheet" href="/d/widget.css"></head>
<body><main id="jc-chat" class="jc-chat" data-config="${attr(JSON.stringify({ ...CONFIG, widget: WIDGET }))}"></main>
<script src="/d/widget.js" defer></script></body></html>`;
const HOST = `<!doctype html><html lang="sk"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"></head>
<body style="margin:0;font:16px sans-serif"><p style="padding:16px">Stránka mesta.</p>
<script src="/d/bb/embed.js" defer></script></body></html>`;
// About three screens of answer, five sources on four sites, and follow-ups.
const LONG = ["The client centre is open on weekdays; the details by service follow.", "",
  ...Array.from({ length: 30 }, (_, i) => `- Answer line ${i + 1} with enough words to wrap at a phone's width [${(i % 5) + 1}]`)].join("\n");
// T-4650: a data answer, the shape src/chat/routed.rs sends: the name first, the values with units, the time last.
const DATA = {
  type: "AirQualityObserved", total: 12, columns: ["name", "pm10", "no2", "observedAt"], units: [null, "µg/m³", "µg/m³", null],
  rows: [["Banská Bystrica, Štefánikovo nábrežie, stanica merania kvality ovzdušia", 21.5, 14, "2026-10-16T12:00:00Z"],
    ["Zvolen, J. Alexyho", null, 9.25, "2026-10-16T11:00:00Z"]],
};
function sse(res, data) {
  res.writeHead(200, { "Content-Type": "text/event-stream" });
  const send = (name, data) => res.write(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);
  send("conversation", { id: "c1", at: "2026-10-16T12:17:05Z" });
  if (data) send("data", DATA);
  send("answer", { text: LONG });
  send("citations", [1, 2, 3, 4, 5].map((n) => ({ n, url: `https://${n < 3 ? "example" : "site" + n}.sk/strana-${n}`, title: `Source ${n}` })));
  send("followups", { questions: ["Where is the client centre?", "Do I need an appointment?"] });
  res.end();
}

// The visitor tokens the stand-in has issued (T-4653).
let TOKENS = 0;

// T-4649: an answer streamed in three-character deltas, split mid-token, with HTML in it.
const STREAMED = ["## Doprava", "", "Máme **tri** zdroje [1, 2]:", "", "- **`type`**: typ entity [1]", "- **`q`**: filter [2, 3]", "",
  "| Zdroj | Stav |", "|---|---|", "| **Parkoviská** | `live` |", "", "```", "GET /ngsi-ld/v1/entities?type=OffStreetParking", "```",
  "Pozor na <script>alert(1)</script> a <img src=x onerror=alert(1)> [docs](https://example.sk/a).", ""].join("\n");
let SLOW = false;
async function slow(res) {
  res.writeHead(200, { "Content-Type": "text/event-stream" });
  const send = (name, data) => res.write(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);
  send("conversation", { id: "c2", at: "2026-10-18T12:00:00Z" });
  for (let at = 0; at < STREAMED.length; at += 3) {
    send("delta", { text: STREAMED.slice(at, at + 3) });
    await new Promise((done) => setTimeout(done, 8));
  }
  // A moment for the last frame, so the test can hold it against the final view.
  await new Promise((done) => setTimeout(done, 100));
  send("answer", { text: STREAMED });
  send("citations", [1, 2, 3].map((n) => ({ n, url: `https://example.sk/strana-${n}`, title: `Source ${n}` })));
  res.end();
}

let server;
let base;
test.beforeAll(async () => {
  server = createServer((req, res) => {
    const path = new URL(req.url, "http://x").pathname;
    if (path === "/d/widget.js") return res.writeHead(200, { "Content-Type": "text/javascript" }).end("var JC_FIELDS = " + read("../../jc-core/data/field-order.json") + ";\n" + read("render.js") + read("widget.js"));
    if (path === "/d/widget.css") return res.writeHead(200, { "Content-Type": "text/css" }).end(read("widget.css"));
    if (path === "/d/bb/widget") return res.writeHead(200, { "Content-Type": "text/html" }).end(PAGE());
    if (path === "/d/bb") return res.writeHead(200, { "Content-Type": "text/html" }).end(PAGE().replace("<body>", '<body class="jc-page">'));
    if (path === "/d/bb/embed.js") return res.writeHead(200, { "Content-Type": "text/javascript" }).end(read("embed.js"));
    if (path === "/host") return res.writeHead(200, { "Content-Type": "text/html" }).end(HOST);
    if (path === "/d/visitor.js") return res.writeHead(200, { "Content-Type": "text/javascript" }).end(read("visitor.js"));
    if (path === "/api/v1/d/bb/challenge") {
      return res.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify({ challenge: "c0ffee.1.8.5a", difficulty: 8, expiresAt: "2099-01-01T00:00:00Z" }));
    }
    if (path === "/api/v1/d/bb/token") {
      let body = "";
      req.on("data", (chunk) => { body += chunk; });
      req.on("end", () => {
        const { challenge, nonce } = JSON.parse(body);
        const ok = challenge === "c0ffee.1.8.5a" && createHash("sha256").update(`${challenge}:${nonce}`).digest()[0] === 0;
        if (!ok) return res.writeHead(400).end();
        TOKENS += 1;
        res.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify({ token: `visitor-${TOKENS}`, expiresAt: "2099-01-01T00:00:00Z" }));
      });
      return;
    }
    if (path === "/api/v1/d/bb/chat") {
      // T-4653: every question carries the token the worker's proof earned; the newest alone holds.
      if (req.headers["x-jc-visitor"] !== `visitor-${TOKENS}`) return res.writeHead(401, { "Content-Type": "application/json" }).end("{}");
      if (SLOW) return slow(res);
      let body = "";
      req.on("data", (chunk) => { body += chunk; });
      return req.on("end", () => sse(res, body.includes("ovzdušia")));
    }
    res.writeHead(404).end();
  });
  await new Promise((done) => server.listen(0, "127.0.0.1", done));
  base = `http://127.0.0.1:${server.address().port}`;
});
test.afterAll(() => server.close());

/** Every element of the chat's page that scrolls, and whether the page itself does. */
async function scrollers(frame) {
  return frame.evaluate(() => ({
    page: document.scrollingElement.scrollHeight > innerHeight + 1,
    areas: [...document.querySelectorAll("*")].filter((el) => {
      const style = getComputedStyle(el);
      return /(auto|scroll)/.test(style.overflowY) && el.scrollHeight > el.clientHeight + 1;
    }).map((el) => el.className),
  }));
}

const inside = (box, width, height) => box && box.x >= 0 && box.y >= 0 && box.x + box.width <= width + 0.5 && box.y + box.height <= height + 0.5;

for (const [width, height] of [[1366, 768], [768, 1024], [390, 844], [320, 640]]) {
  test(`the panel at ${width}x${height}: the browser's language at once, one scroll area, nothing cut`, async ({ page }) => {
    WIDGET = undefined;
    await page.setViewportSize({ width, height });
    await page.goto(`${base}/host`);
    const launcher = page.locator("[data-jc-assistant]").locator(".jc-launch");
    expect(inside(await launcher.boundingBox(), width, height), "the closed launcher is whole").toBe(true);
    await launcher.click();
    const frame = page.frameLocator("[data-jc-assistant] iframe");

    // T-4343: the first open asks nothing; this browser is en-US, so the chat speaks English.
    await expect(frame.locator("#jc-question")).toBeVisible();
    await expect(frame.locator(".jc-choose-list")).toHaveCount(0);
    await expect(frame.locator(".jc-greeting")).toBeVisible();
    await expect(frame.locator(".jc-langs button[aria-pressed=true]")).toHaveText("EN");
    await expect(frame.locator(".jc-suggest button")).toHaveCount(3);
    await expect(frame.locator("select")).toHaveCount(0);

    // T-4592: one tall panel, 760 px or the window less the launcher; the whole screen on a phone.
    // A fresh chat reads from the top: the greeting under the header, the composer whole.
    const panel = page.locator("[data-jc-assistant] .jc-panel");
    const opened = await panel.boundingBox();
    if (width > 640) {
      expect(inside(opened, width, height), "the panel fits the window").toBe(true);
      expect(Math.round(opened.height), "the panel is 760 px or the window less 108").toBe(Math.min(760, height - 108));
    } else {
      expect([opened.x, opened.y, Math.round(opened.width), Math.round(opened.height)], "the panel fills the phone").toEqual([0, 0, width, height]);
      expect(inside(await frame.locator(".jc-form").boundingBox(), width, height), "the composer is in view").toBe(true);
    }
    await expect.poll(async () => {
      const top = await frame.locator(".jc-head").boundingBox();
      const greeting = await frame.locator(".jc-greeting").boundingBox();
      return greeting.y - (top.y + top.height);
    }, { message: "the greeting sits under the header" }).toBeLessThanOrEqual(24);

    // T-4635 option C: a labelled send button inside the bar, no circle in the footer.
    const bar = await frame.locator(".jc-form").boundingBox();
    const send = frame.locator(".jc-form .jc-submit");
    await expect(send).toHaveText("Send");
    const sendBox = await send.boundingBox();
    expect(inside({ x: sendBox.x - bar.x, y: sendBox.y - bar.y, width: sendBox.width, height: sendBox.height }, bar.width, bar.height),
      "the send button lies inside the bar").toBe(true);
    const circles = await frame.locator(".jc-foot").evaluate((foot) => [...foot.querySelectorAll("*")].filter((el) => {
      const box = el.getBoundingClientRect();
      const radius = parseFloat(getComputedStyle(el).borderTopLeftRadius);
      return box.width > 4 && Math.abs(box.width - box.height) < 2 && radius >= box.width / 2 - 1;
    }).map((el) => el.className));
    expect(circles, "no circle in the footer").toEqual([]);
    // T-4670: the footer is one line, whole and wrapping; the disclaimer moved into the privacy dialog.
    const line = await frame.locator(".jc-privacy-line").evaluate((p) => ({
      text: p.textContent, wraps: getComputedStyle(p).whiteSpace !== "nowrap", cut: getComputedStyle(p).textOverflow === "ellipsis" }));
    expect(line, "the footer line is whole").toEqual({ text: "Please verify important information. · Privacy", wraps: true, cut: false });
    await expect(frame.locator(".jc-disclaimer")).toHaveCount(0);

    // The name and its subtitle keep one line each, whole, at every size.
    const header = await frame.locator(".jc-title").evaluate((title) => {
      const name = title.querySelector(".jc-name");
      return { whole: name.scrollWidth <= name.clientWidth, lines: Math.round(name.offsetHeight / parseFloat(getComputedStyle(name).lineHeight)),
        role: getComputedStyle(title.querySelector(".jc-role")).display !== "none" };
    });
    // 320 px is here for the footer row (T-4364); the header's name is cut there today, its own finding.
    if (width >= 375) expect(header, "the header name is one whole line with its subtitle").toEqual({ whole: true, lines: 1, role: true });

    // A three-screen answer: the conversation scrolls, the page does not, header and composer stay.
    await frame.locator("#jc-question").fill("Kedy má otvorené klientske centrum?");
    await frame.locator("#jc-question").press("Enter");
    await expect(frame.locator(".jc-followups button")).toHaveCount(2);
    // T-4364: under the answer only one row; the five sources wait behind its count.
    const bot = frame.locator(".jc-bot").last();
    await expect(bot.locator(".jc-cards")).toBeHidden();
    await expect(bot.locator(".jc-cards > li")).toHaveCount(5);
    const footer = await bot.locator(".jc-answer-tools").evaluate((row) =>
      Array.from(row.children, (c) => c.classList[c.classList.length - 1] + (c.classList.contains("jc-thumb") ? ":" + c.getAttribute("aria-label") : "")));
    expect(footer, "who and when, copy, thumbs, then the sources").toEqual(
      ["jc-by", "jc-copy", "jc-thumb:Helpful answer", "jc-thumb:Not helpful", "jc-sources"]);
    await expect(bot.locator(".jc-by")).toHaveText(/^AI assistant • \d{2}:\d{2}$/);
    const toggle = bot.locator(".jc-sources");
    await expect(toggle.locator(".jc-count")).toHaveText("5 sources");
    await expect(toggle).toHaveAccessibleName("5 sources");
    await expect(toggle).toHaveAttribute("aria-expanded", "false");
    await expect(toggle.locator(".jc-badge")).toHaveText(["E", "S", "S"]);
    const listId = await toggle.getAttribute("aria-controls");
    await expect(bot.locator(`#${listId}`)).toHaveAttribute("aria-labelledby", await toggle.getAttribute("id"));
    // The count is one line beside its badges at every width, 320 px included.
    const count = await toggle.evaluate((b) => {
      const c = b.querySelector(".jc-count");
      return { lines: Math.round(c.getBoundingClientRect().height / parseFloat(getComputedStyle(c).lineHeight)), whole: b.scrollWidth <= b.clientWidth + 1 };
    });
    expect(count, "the count does not wrap").toEqual({ lines: 1, whole: true });
    // T-4339: the question shows the server's moment in the visitor's zone (Europe/Bratislava
    // here, layout.config.mjs), the owner's "14:17" beside "Je piatok 16. októbra 2026, 14:17 (CEST)".
    const asked = frame.locator(".jc-you .jc-time").first();
    await expect(asked).toHaveText("14:17");
    await expect(asked).toHaveAttribute("datetime", "2026-10-16T12:17Z");
    await expect(asked).toHaveAttribute("title", "2026-10-16 12:17 UTC");
    for (const who of [".jc-you", ".jc-bot"]) {
      const time = frame.locator(`${who} .jc-time`).first();
      await expect(time).toHaveText(/^\d{1,2}[:.]\d{2}/);
      await expect(time).toHaveAttribute("title", /^\d{4}-\d{2}-\d{2} \d{2}:\d{2} UTC$/);
    }
    // The count opens the cards under the row and closes them again; a marker opens them too.
    await toggle.click();
    await expect(bot.locator(".jc-cards > li:visible")).toHaveCount(5);
    await expect(toggle).toHaveAttribute("aria-expanded", "true");
    const under = await bot.evaluate((b) => b.querySelector(".jc-answer-tools").getBoundingClientRect().bottom <= b.querySelector(".jc-cards").getBoundingClientRect().top);
    expect(under, "the cards open under the row").toBe(true);
    await toggle.click();
    await expect(bot.locator(".jc-cards")).toBeHidden();
    await expect(toggle).toHaveAttribute("aria-expanded", "false");
    await bot.locator(".jc-cite a").first().click();
    await expect(bot.locator(".jc-cards")).toBeVisible();
    await expect(toggle).toHaveAttribute("aria-expanded", "true");
    // One line per source: the number, the title and the host share a row.
    const row = await frame.locator(".jc-cards > li").first().evaluate((li) => {
      const [a, host] = [li.querySelector("a"), li.querySelector(".jc-domain")];
      return Math.abs(a.getBoundingClientRect().top - host.getBoundingClientRect().top) < 6;
    });
    expect(row, "a source is one row").toBe(true);
    const chat = page.frames().find((f) => f.url().includes("/d/bb/widget"));
    await chat.evaluate(() => { const log = document.querySelector(".jc-log"); log.scrollTop = log.scrollHeight; });
    const scroll = await scrollers(chat);
    expect(scroll.page, "the chat's page never scrolls").toBe(false);
    expect(scroll.areas, "the conversation is the one scroll area").toEqual(["jc-log"]);
    // Boxes are in the page's coordinates: each part lies within the frame, the frame within the window.
    const frameBox = await page.locator("[data-jc-assistant] iframe").boundingBox();
    expect(inside(frameBox, width, height), "the frame lies within the window").toBe(true);
    for (const part of [".jc-head", ".jc-form", ".jc-privacy-line"]) {
      const box = await frame.locator(part).boundingBox();
      expect(box.y >= frameBox.y - 0.5 && box.y + box.height <= frameBox.y + frameBox.height + 0.5, `${part} is whole`).toBe(true);
    }
    const head = await frame.locator(".jc-head").boundingBox();
    expect(head.y, "the header stays at the top after a long answer").toBeLessThanOrEqual(frameBox.y + 1);
    expect(await panel.boundingBox(), "a long answer leaves the panel as it opened").toEqual(opened);
    if (width <= 480) {
      const controls = [".jc-head button", ".jc-form button", ".jc-answer-tools button", ".jc-cards > li", ".jc-followups button", ".jc-privacy-line .jc-more"].map((s) => s + ":visible").join(", ");
      for (const control of await frame.locator(controls).all()) {
        const box = await control.boundingBox();
        expect(Math.min(box.width, box.height), "a tap target of 44 px").toBeGreaterThanOrEqual(44);
      }
    }

    // A second open in this browser goes straight to the chat, in the language chosen.
    await page.reload();
    await page.locator("[data-jc-assistant] .jc-launch").click();
    await expect(frame.locator("#jc-question")).toBeVisible();
    await expect(frame.locator(".jc-choose-list")).toHaveCount(0);
    await expect(frame.locator(".jc-langs button[aria-pressed=true]")).toHaveText("EN");
  });
}

test("the full page that names ?lang= does not ask", async ({ page }) => {
  await page.goto(`${base}/d/bb?lang=en`);
  await expect(page.locator("#jc-question")).toBeVisible();
  await expect(page.locator(".jc-choose-list")).toHaveCount(0);
  await expect(page.locator(".jc-langs button[aria-pressed=true]")).toHaveText("EN");
});

// T-4650: at a phone's width the table reads from the left: the name, then the first value whole.
test("a data table at 400 px shows the name and the first value", async ({ page }) => {
  await page.setViewportSize({ width: 400, height: 800 });
  await page.goto(`${base}/d/bb?lang=sk`);
  await page.locator("#jc-question").fill("Aká je kvalita ovzdušia?");
  await page.locator(".jc-form .jc-submit").click();
  const table = page.locator(".jc-data");
  await expect(table.locator("th").first()).toHaveText("Názov");
  await expect(table.locator("th").nth(1)).toHaveText("PM10 (µg/m³)");
  await expect(table.locator("th").last()).toHaveText("Čas merania");
  await expect(table.locator("tbody tr").first().locator("td").nth(1)).toHaveText("21,5");
  await expect(table.locator("tbody tr").nth(1).locator("td").nth(1)).toHaveText("–");
  const scroll = await table.locator(".jc-data-scroll").boundingBox();
  const value = await table.locator("tbody tr").first().locator("td").nth(1).boundingBox();
  expect(value.x + value.width, "the first value lies in view without scrolling").toBeLessThanOrEqual(scroll.x + scroll.width + 0.5);
  expect(await table.locator("td.jc-num").count(), "values align as numbers").toBeGreaterThan(0);
});

// T-4343: the deployment's widget.language decides; the browser's languages never leave the page.
test("a Slovak browser opens in Slovak with no question", async ({ browser }) => {
  WIDGET = { language: "browser" };
  const context = await browser.newContext({ locale: "sk-SK" });
  const page = await context.newPage();
  await page.goto(`${base}/d/bb`);
  await expect(page.locator("#jc-question")).toBeVisible();
  await expect(page.locator(".jc-choose-list")).toHaveCount(0);
  await expect(page.locator(".jc-langs button[aria-pressed=true]")).toHaveText("SK");
  // The toggle still switches, and the pick wins over the browser on the next open.
  await page.locator(".jc-langs button", { hasText: "EN" }).click();
  await expect(page.locator(".jc-langs button[aria-pressed=true]")).toHaveText("EN");
  await page.reload();
  await expect(page.locator(".jc-langs button[aria-pressed=true]")).toHaveText("EN");
  await context.close();
});

test("ask mode keeps the first-open chooser, Slovak first and focused", async ({ page }) => {
  WIDGET = { language: "ask" };
  await page.goto(`${base}/host`);
  await page.locator("[data-jc-assistant] .jc-launch").click();
  const frame = page.frameLocator("[data-jc-assistant] iframe");
  const choices = frame.locator(".jc-choose-list button");
  await expect(choices).toHaveText(["Slovensky", "English"]);
  await expect(choices.first()).toBeFocused();
  await expect(frame.locator("#jc-question")).toBeHidden();
  // The chooser is never a dead end: its header keeps the close button.
  await expect(frame.locator(".jc-head .jc-close")).toBeVisible();
  await expect(frame.locator(".jc-head .jc-langs")).toBeHidden();
  await choices.nth(1).click();
  await expect(frame.locator(".jc-langs button[aria-pressed=true]")).toHaveText("EN");
  await expect(frame.locator("#jc-question")).toBeVisible();
});

test("fixed mode ignores the browser, and languageToggle false hides the toggle", async ({ page }) => {
  WIDGET = { language: "fixed" };
  await page.goto(`${base}/d/bb`);
  await expect(page.locator(".jc-choose-list")).toHaveCount(0);
  await expect(page.locator(".jc-langs button[aria-pressed=true]")).toHaveText("SK");
  WIDGET = { language: "browser", languageToggle: false };
  await page.goto(`${base}/d/bb`);
  await expect(page.locator("#jc-question")).toBeVisible();
  await expect(page.locator(".jc-langs")).toHaveCount(0);
  // Without the toggle the en-US browser still picks English.
  await expect(page.locator("#jc-question")).toHaveAttribute("placeholder", "Type your question");
});

test("a token the service forgot is earned again once, and the question is answered (T-4653)", async ({ page }) => {
  WIDGET = undefined;
  await page.goto(`${base}/d/bb?lang=en`);
  await expect.poll(() => TOKENS, { message: "the worker earned a token on open" }).toBeGreaterThan(0);
  const before = TOKENS;
  TOKENS += 1; // a restart: the token the page holds is no longer known
  await page.locator("#jc-question").fill("When is the client centre open?");
  await page.locator("#jc-question").press("Enter");
  await expect(page.locator(".jc-answer-tools")).toBeVisible();
  expect(TOKENS, "one new token, after the 401").toBe(before + 2);
  await expect(page.locator(".jc-error")).toHaveCount(0);
});

test("a streamed answer is formatted while it streams, never raw markers, and ends as it streamed (T-4649)", async ({ page }) => {
  WIDGET = undefined;
  SLOW = true;
  await page.goto(`${base}/d/bb?lang=en`);
  // Every state of the answer while it streams, read on each change of the log.
  await page.evaluate(() => {
    window.frames_ = [];
    new MutationObserver(() => {
      const streaming = document.querySelector(".jc-streaming");
      if (!streaming) return;
      const copy = streaming.cloneNode(true);
      copy.querySelectorAll("pre").forEach((pre) => pre.remove());
      window.frames_.push({ text: copy.innerText, html: streaming.innerHTML });
    }).observe(document.querySelector(".jc-log") || document.body, { subtree: true, childList: true, characterData: true });
  });
  await page.locator("#jc-question").fill("Aké dáta máte o doprave?");
  await page.locator("#jc-question").press("Enter");
  await expect(page.locator(".jc-answer-tools")).toBeVisible();
  SLOW = false;
  const frames = await page.evaluate(() => window.frames_);
  expect(frames.length, "the answer was seen while streaming").toBeGreaterThan(10);
  for (const frame of frames) {
    expect(frame.text, "no frame shows a Markdown marker").not.toMatch(/\*\*|`|^\s*\||^#|\[\d|\]\(/m);
  }
  const final = page.locator(".jc-bot .jc-answer").last();
  expect(await final.locator("script, img").count(), "HTML in the answer is never parsed").toBe(0);
  await expect(final).toContainText("<script>alert(1)</script>");
  await expect(final.locator("table th")).toHaveText(["Zdroj", "Stav"]);
  await expect(final.locator("li strong code")).toHaveText(["type", "q"]);
  await expect(final.locator("sup.jc-cite a").first()).toBeVisible();
  const settled = await final.evaluate((answer) => {
    const copy = answer.cloneNode(true);
    copy.querySelectorAll("sup.jc-cite").forEach((sup) => sup.remove());
    return copy.innerHTML.replace(/<sup class="jc-cite"><\/sup>/g, "");
  });
  const last = frames[frames.length - 1].html.replace(/<sup class="jc-cite"><\/sup>/g, "");
  expect(last, "the last streamed frame is the final answer without its markers").toBe(settled);
});

test("the panel expands to the window and back: button, Escape, focus kept, the page held still (T-4658)", async ({ page }) => {
  WIDGET = undefined;
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto(`${base}/host`);
  // A message that is not our frame's, from the page itself and from a frame of another origin.
  await page.evaluate((other) => {
    const foreign = document.createElement("iframe");
    foreign.src = other + "/d/bb/widget";
    document.body.prepend(foreign);
  }, base.replace("127.0.0.1", "localhost"));
  const panel = page.locator("[data-jc-assistant] .jc-panel");
  await page.locator("[data-jc-assistant] .jc-launch").click();
  const chat = page.frameLocator("[data-jc-assistant] iframe");
  const size = chat.locator(".jc-size");
  await expect(size).toHaveAttribute("aria-label", "Expand");
  await page.evaluate(() => window.postMessage({ type: "jc-assistant-size", size: "full" }, "*"));
  await page.frames().find((f) => f.url().startsWith("http://localhost")).evaluate(() =>
    window.parent.postMessage({ type: "jc-assistant-size", size: "full" }, "*"));
  await page.waitForTimeout(200);
  await expect(panel).not.toHaveClass(/jc-full/);

  await size.click();
  await expect(panel).toHaveClass(/jc-full/);
  await expect(size).toHaveAttribute("aria-label", "Collapse");
  await expect(size).toHaveAttribute("aria-pressed", "true");
  const box = await panel.boundingBox();
  expect(box.width).toBe(960);
  expect(Math.round(box.x)).toBe(160);
  expect(box.y).toBe(20);
  expect(box.height).toBe(760);
  await expect(page.locator("[data-jc-assistant] .jc-launch")).toBeHidden();
  // The page behind is inert and does not scroll.
  expect(await page.evaluate(() => [document.querySelector("p").inert, document.documentElement.style.overflow])).toEqual([true, "hidden"]);
  // Focus stays in the chat: Tab from its last control does not reach the page.
  await chat.locator("textarea").focus();
  for (let i = 0; i < 12; i++) await page.keyboard.press("Tab");
  expect(await page.evaluate(() => document.activeElement && document.activeElement.hasAttribute("data-jc-assistant"))).toBe(true);
  // The state survives a reload of the frame in this tab.
  expect(await page.frames().find((f) => f.url().includes("127.0.0.1") && f.url().includes("/widget")).evaluate(() =>
    sessionStorage.getItem("jc-chat-size:bb"))).toBe("full");

  // Escape collapses first, then closes.
  await chat.locator("textarea").press("Escape");
  await expect(panel).not.toHaveClass(/jc-full/);
  await expect(panel).toBeVisible();
  expect((await panel.boundingBox()).width).toBe(420);
  expect(await page.evaluate(() => [document.querySelector("p").inert, document.documentElement.style.overflow])).toEqual([false, ""]);
  await expect(size).toHaveAttribute("aria-label", "Expand");
  await chat.locator("textarea").press("Escape");
  await expect(panel).toBeHidden();

  // Opened again expanded by the button and collapsed by it.
  await page.locator("[data-jc-assistant] .jc-launch").click();
  await size.click();
  await expect(panel).toHaveClass(/jc-full/);
  await size.click();
  await expect(panel).not.toHaveClass(/jc-full/);
});

test("on a phone the panel already fills the screen and offers no expand button (T-4658)", async ({ page }) => {
  WIDGET = undefined;
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`${base}/host`);
  await page.locator("[data-jc-assistant] .jc-launch").click();
  const chat = page.frameLocator("[data-jc-assistant] iframe");
  await expect(chat.locator(".jc-close")).toBeVisible();
  await expect(chat.locator(".jc-size")).toHaveCount(0);
});

// T-4670: the footer's privacy link opens a modal dialog that holds focus and closes on Escape
// alone; its switch keeps the conversation in localStorage for 90 days, and off deletes it.
test("the privacy dialog holds focus, closes on Escape, and its switch keeps or deletes the browser's copy (T-4670)", async ({ page, context }) => {
  WIDGET = undefined;
  SLOW = false;
  await page.setViewportSize({ width: 420, height: 720 });
  await page.goto(`${base}/d/bb?lang=sk`);
  await expect(page.locator(".jc-privacy-line")).toHaveText("Dôležité informácie si prosím overte. · Súkromie");
  const open = page.locator(".jc-privacy-line .jc-more");
  await open.click();
  const dialog = page.locator("dialog.jc-about");
  await expect(dialog).toBeVisible();
  await expect(dialog.locator("h2")).toHaveText("O tomto asistentovi");
  await expect(dialog.locator("p").first()).toHaveText(CONFIG.disclaimer.sk);
  await expect(dialog).toContainText("30 dní");
  await expect(dialog.locator('a[href="mailto:dpo@banskabystrica.sk"]')).toBeVisible();
  await expect(dialog.locator('a[href="https://www.banskabystrica.sk/gdpr"]')).toBeVisible();
  const box = await dialog.boundingBox();
  expect(box.x).toBeGreaterThanOrEqual(0);
  expect(box.x + box.width).toBeLessThanOrEqual(420);
  for (const key of ["Tab", "Tab", "Tab", "Tab", "Tab", "Tab", "Shift+Tab", "Shift+Tab", "Shift+Tab", "Shift+Tab", "Shift+Tab"]) {
    await page.keyboard.press(key);
    expect(await page.evaluate(() => Boolean(document.activeElement && document.activeElement.closest("dialog")))).toBe(true);
  }
  const toggle = dialog.locator('input[role="switch"]');
  await expect(toggle).not.toBeChecked();
  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
  await expect(page.locator("#jc-question")).toBeVisible();
  await expect(open).toBeFocused();

  const KEY = "jc-chat-remember:bb";
  expect(await page.evaluate((k) => localStorage.getItem(k), KEY)).toBeNull();
  await open.click();
  await toggle.check();
  expect(JSON.parse(await page.evaluate((k) => localStorage.getItem(k), KEY)).kept).toBeNull();

  // The copy a later visit finds: shown in a new tab (no sessionStorage), the switch still on.
  const conversation = {
    conversation: "00000000-0000-4000-8000-000000000001", asked: 1,
    turns: [{ question: "Kedy má otvorené klientske centrum?", text: "V pracovné dni od 8:00.", turn: 1, citations: [] }],
  };
  await page.evaluate(([k, kept]) => localStorage.setItem(k, JSON.stringify({ savedAt: new Date().toISOString(), kept })), [KEY, conversation]);
  const later = await context.newPage();
  await later.goto(`${base}/d/bb?lang=sk`);
  await expect(later.locator(".jc-log")).toContainText("V pracovné dni od 8:00.");
  await later.locator(".jc-privacy-line .jc-more").click();
  const laterToggle = later.locator('dialog.jc-about input[role="switch"]');
  await expect(laterToggle).toBeChecked();
  await laterToggle.uncheck();
  expect(await later.evaluate((k) => localStorage.getItem(k), KEY)).toBeNull();
  await later.close();

  // Past 90 days the copy is not shown and is deleted on the visit.
  const old = new Date(Date.now() - 91 * 24 * 3600 * 1000).toISOString();
  await page.evaluate(([k, kept, at]) => localStorage.setItem(k, JSON.stringify({ savedAt: at, kept })), [KEY, conversation, old]);
  const stale = await context.newPage();
  await stale.goto(`${base}/d/bb?lang=sk`);
  await expect(stale.locator("#jc-question")).toBeVisible();
  await expect(stale.locator(".jc-log")).not.toContainText("V pracovné dni od 8:00.");
  expect(await stale.evaluate((k) => localStorage.getItem(k), KEY)).toBeNull();
});
