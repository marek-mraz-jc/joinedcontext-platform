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
const PAGE = `<!doctype html><html lang="sk"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><link rel="stylesheet" href="/d/widget.css"></head>
<body><main id="jc-chat" class="jc-chat" data-config="${attr(JSON.stringify(CONFIG))}"></main>
<script src="/d/widget.js" defer></script></body></html>`;
const HOST = `<!doctype html><html lang="sk"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"></head>
<body style="margin:0;font:16px sans-serif"><p style="padding:16px">Stránka mesta.</p>
<script src="/d/bb/embed.js" defer></script></body></html>`;
// About three screens of answer, four sources, and follow-ups.
const LONG = Array.from({ length: 30 }, (_, i) => `- Riadok odpovede ${i + 1} s dostatkom slov, aby sa zalomil [${(i % 4) + 1}]`).join("\n");
function sse(res) {
  res.writeHead(200, { "Content-Type": "text/event-stream" });
  const send = (name, data) => res.write(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);
  send("conversation", { id: "c1" });
  send("answer", { text: LONG });
  send("citations", [1, 2, 3, 4].map((n) => ({ n, url: `https://example.sk/strana-${n}`, title: `Zdroj ${n}` })));
  send("followups", { questions: ["Kde je klientske centrum?", "Treba sa objednať?"] });
  res.end();
}

let server;
let base;
test.beforeAll(async () => {
  server = createServer((req, res) => {
    const path = new URL(req.url, "http://x").pathname;
    if (path === "/d/widget.js") return res.writeHead(200, { "Content-Type": "text/javascript" }).end(read("render.js") + read("widget.js"));
    if (path === "/d/widget.css") return res.writeHead(200, { "Content-Type": "text/css" }).end(read("widget.css"));
    if (path === "/d/bb/widget") return res.writeHead(200, { "Content-Type": "text/html" }).end(PAGE);
    if (path === "/d/bb") return res.writeHead(200, { "Content-Type": "text/html" }).end(PAGE.replace("<body>", '<body class="jc-page">'));
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

for (const [width, height] of [[1280, 720], [768, 1024], [375, 812]]) {
  test(`the panel at ${width}x${height}: chooser once, one scroll area, nothing cut`, async ({ page }) => {
    await page.setViewportSize({ width, height });
    await page.goto(`${base}/host`);
    const launcher = page.locator("[data-jc-assistant]").locator(".jc-launch");
    expect(inside(await launcher.boundingBox(), width, height), "the closed launcher is whole").toBe(true);
    await launcher.click();
    const frame = page.frameLocator("[data-jc-assistant] iframe");

    // First open: the language before anything else, Slovak first and focused.
    const choices = frame.locator(".jc-choose-list button");
    await expect(choices).toHaveText(["Slovensky", "English"]);
    await expect(choices.first()).toBeFocused();
    await expect(frame.locator("#jc-question")).toBeHidden();
    await choices.nth(1).click();
    await expect(frame.locator(".jc-langs button[aria-pressed=true]")).toHaveText("EN");
    await expect(frame.locator(".jc-suggest button")).toHaveCount(3);
    await expect(frame.locator("select")).toHaveCount(0);

    // A short conversation leaves no empty panel: the chips sit right above the composer.
    const panel = page.locator("[data-jc-assistant] .jc-panel");
    await expect.poll(async () => {
      const chips = await frame.locator(".jc-suggest").boundingBox();
      const composer = await frame.locator(".jc-form").boundingBox();
      return composer.y - (chips.y + chips.height);
    }, { message: "no dead space under the chips" }).toBeLessThan(40);
    if (width > 640) {
      expect(inside(await panel.boundingBox(), width, height), "the panel fits the window").toBe(true);
      // ...and is as tall as what it shows: no empty band under the header either.
      await expect.poll(async () => {
        const top = await frame.locator(".jc-head").boundingBox();
        const greeting = await frame.locator(".jc-greeting").boundingBox();
        return greeting.y - (top.y + top.height);
      }, { message: "the panel fits its content" }).toBeLessThan(40);
    }

    // A three-screen answer: the conversation scrolls, the page does not, header and composer stay.
    await frame.locator("#jc-question").fill("Kedy má otvorené klientske centrum?");
    await frame.locator("#jc-question").press("Enter");
    await expect(frame.locator(".jc-followups button")).toHaveCount(2);
    await expect(frame.locator(".jc-cards > li:visible")).toHaveCount(2);
    await frame.locator(".jc-more-sources").click();
    await expect(frame.locator(".jc-cards > li:visible")).toHaveCount(4);
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
    if (width > 640) expect(inside(await panel.boundingBox(), width, height), "the grown panel still fits").toBe(true);
    if (width <= 480) {
      for (const control of await frame.locator(".jc-head button, .jc-form button:visible").all()) {
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
