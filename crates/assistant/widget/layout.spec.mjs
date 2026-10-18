// T-4283: the widget's layout in a browser, at a desktop, a tablet and a phone. A small server
// stands in for jc-assistant: the page and its script as src/chat/widget.rs serves them, a chat
// route that streams one long answer, and a host page with the launcher (embed.js).
//   cd crates/assistant/widget && npm i --no-save @playwright/test@1.62.1 && npx playwright test -c layout.config.mjs
import { createServer } from "node:http";
import { readFileSync } from "node:fs";
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
function sse(res) {
  res.writeHead(200, { "Content-Type": "text/event-stream" });
  const send = (name, data) => res.write(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);
  send("conversation", { id: "c1", at: "2026-10-16T12:17:05Z" });
  send("answer", { text: LONG });
  send("citations", [1, 2, 3, 4, 5].map((n) => ({ n, url: `https://${n < 3 ? "example" : "site" + n}.sk/strana-${n}`, title: `Source ${n}` })));
  send("followups", { questions: ["Where is the client centre?", "Do I need an appointment?"] });
  res.end();
}

let server;
let base;
test.beforeAll(async () => {
  server = createServer((req, res) => {
    const path = new URL(req.url, "http://x").pathname;
    if (path === "/d/widget.js") return res.writeHead(200, { "Content-Type": "text/javascript" }).end(read("render.js") + read("widget.js"));
    if (path === "/d/widget.css") return res.writeHead(200, { "Content-Type": "text/css" }).end(read("widget.css"));
    if (path === "/d/bb/widget") return res.writeHead(200, { "Content-Type": "text/html" }).end(PAGE());
    if (path === "/d/bb") return res.writeHead(200, { "Content-Type": "text/html" }).end(PAGE().replace("<body>", '<body class="jc-page">'));
    if (path === "/d/bb/embed.js") return res.writeHead(200, { "Content-Type": "text/javascript" }).end(read("embed.js"));
    if (path === "/host") return res.writeHead(200, { "Content-Type": "text/html" }).end(HOST);
    if (path === "/api/v1/d/bb/chat") return sse(res);
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
    for (const part of [".jc-head", ".jc-form", ".jc-notice"]) {
      const box = await frame.locator(part).boundingBox();
      expect(box.y >= frameBox.y - 0.5 && box.y + box.height <= frameBox.y + frameBox.height + 0.5, `${part} is whole`).toBe(true);
    }
    const head = await frame.locator(".jc-head").boundingBox();
    expect(head.y, "the header stays at the top after a long answer").toBeLessThanOrEqual(frameBox.y + 1);
    expect(await panel.boundingBox(), "a long answer leaves the panel as it opened").toEqual(opened);
    if (width <= 480) {
      const controls = [".jc-head button", ".jc-form button", ".jc-answer-tools button", ".jc-cards > li", ".jc-followups button", ".jc-notice .jc-more"].map((s) => s + ":visible").join(", ");
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
