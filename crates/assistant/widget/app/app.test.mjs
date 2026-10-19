// T-4694: the standalone assistant. Run with `node --test crates/assistant/widget/app`.
import test from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { bundle, BUDGET } from "./build.mjs";

const { deployment, framedBy } = createRequire(import.meta.url)("./app.js");
const API = "https://assistant.dev.joinedcontext.com";

test("the page's address names the deployment, and nothing else is one", () => {
  assert.equal(deployment("/bb-doprava", ""), "bb-doprava");
  assert.equal(deployment("/bb-doprava/", "?embed=1"), "bb-doprava");
  assert.equal(deployment("/", "?d=bbsk"), "bbsk");
  for (const bad of ["/", "/Upper", "/-x", "/a_b", "/" + "a".repeat(64), "/../etc"]) assert.equal(deployment(bad, ""), null, bad);
});

test("a framed copy shows only inside one of the deployment's origins", () => {
  const origins = ["https://www.banskabystrica.sk"];
  assert.equal(framedBy(["https://www.banskabystrica.sk"], "", origins), true);
  assert.equal(framedBy(undefined, "https://www.banskabystrica.sk/doprava?x=1", origins), true, "the referrer where ancestorOrigins is missing");
  assert.equal(framedBy(["https://evil.example"], "https://www.banskabystrica.sk/", origins), false, "the browser's record wins");
  assert.equal(framedBy([], "", origins), false, "no parent named");
  assert.equal(framedBy(["https://www.banskabystrica.sk.evil.example"], "", origins), false);
  assert.equal(framedBy(["https://www.banskabystrica.sk"], "", []), false, "no origin, no frame");
});

test("the bundle: the page's policy runs only its own scripts, asks only the chat's host, fits its budget and holds no secret", () => {
  const { files, size } = bundle(API, "https://viz.dev.joinedcontext.com");
  const page = files["index.html"];
  const policy = /http-equiv="Content-Security-Policy" content="([^"]+)"/.exec(page)[1];
  assert.match(policy, /script-src 'self';/);
  assert.match(policy, new RegExp(`connect-src ${API};`));
  assert.match(policy, /frame-src https:\/\/viz\.dev\.joinedcontext\.com;/);
  assert.doesNotMatch(policy, /unsafe|\*/);
  assert.doesNotMatch(page, /<script>|<script [^s]|__API__|__FRAMES__|style=/, "no inline script or style, no placeholder left");
  assert.ok(size <= BUDGET, `${size} bytes gzip`);
  const all = Object.values(files).join("\n");
  assert.doesNotMatch(all, /sk-or-v1-|client_secret|BEGIN [A-Z ]*PRIVATE KEY|systemPrompt/);
  assert.doesNotMatch(files["app.js"] + files["widget.js"], /localStorage\.setItem\([^)]*[Tt]oken/, "the visitor token is never stored");
  assert.doesNotMatch(bundle(API).files["index.html"], /frame-src/, "no chart renderer, no frame");
  for (const bad of ["http://assistant.example", "https://assistant.example/path", "", "https://a b"]) {
    assert.throws(() => bundle(bad), /https origin/, bad);
  }
});
